//! The agent's networking core: dial the controller, complete the auth
//! handshake, then service control + session streams until the link drops, with
//! exponential reconnect backoff.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use libretether_common::{pipe_bidirectional, Backoff};
use libretether_protocol::crypto::{self, Identity};
use libretether_protocol::frame::{read_frame_capped, write_frame, MAX_CONTROL_FRAME};
use libretether_protocol::relay::{client_handshake, RelayRole};
use libretether_protocol::{
	tls, Challenge, ControlRequest, Hello, HelloAck, LogLevel, LogLine, LogsResult, SessionGrant, StreamAuth,
	StreamOpen, PROTOCOL_VERSION,
};
use quinn::{Endpoint, RecvStream, SendStream};

/// Shared set of capability tokens issued (one per completed handshake) over a
/// single connection's lifetime; a control/session/tunnel stream must present one.
type TokenSet = Arc<Mutex<HashSet<String>>>;

use crate::config::AgentConfig;
use crate::{handlers, host, session};

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Upper bound on the reconnect backoff, so a long-idle agent still retries often.
const RECONNECT_MAX_SECS: u64 = 5;
/// How long a single dial attempt may hang before we give up and retry.
const CONNECT_TIMEOUT_SECS: u64 = 8;
/// A connection that stayed up at least this long counts as a healthy session,
/// so the next drop retries promptly rather than inheriting a grown backoff.
const RECONNECT_STABLE_SECS: u64 = 30;

static LOG_FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

/// How many recent log lines the agent keeps in memory for [`ControlRequest::FetchLogs`].
/// Older lines are evicted (and the snapshot's `dropped` flag is set) so the buffer
/// can't grow without bound regardless of how long the agent has been running.
const LOG_BUFFER_CAP: usize = 2000;

/// In-memory ring of recent log lines, served to the controller's Logs page via
/// `FetchLogs`. Separate from `agent.log` (which is the on-disk mirror) so the
/// controller can read logs over the link without touching the client's disk.
#[derive(Default)]
struct LogRing {
	lines: std::collections::VecDeque<LogLine>,
	dropped: bool,
}

impl LogRing {
	/// Append `line`, evicting the oldest and flagging `dropped` once `cap` is reached.
	fn push(&mut self, line: LogLine, cap: usize) {
		if self.lines.len() >= cap {
			self.lines.pop_front();
			self.dropped = true;
		}
		self.lines.push_back(line);
	}

	/// The most recent `max` lines (all when `None`), oldest first. Stamps the
	/// agent's current clock so the controller can re-anchor the line timestamps to
	/// its own (the agent may be in another timezone or have a skewed clock).
	fn snapshot(&self, max: Option<usize>) -> LogsResult {
		let take = max.unwrap_or(self.lines.len()).min(self.lines.len());
		LogsResult {
			lines: self.lines.iter().skip(self.lines.len() - take).cloned().collect(),
			dropped: self.dropped,
			agent_now_secs: host::now_secs(),
		}
	}
}

fn log_ring() -> &'static Mutex<LogRing> {
	static RING: OnceLock<Mutex<LogRing>> = OnceLock::new();
	RING.get_or_init(|| Mutex::new(LogRing::default()))
}

/// Snapshot the most recent `max` log lines (all of them when `max` is `None`),
/// oldest first. `dropped` is true if the ring has evicted lines at any point, so
/// the controller can flag that the returned history is partial.
pub fn recent_logs(max: Option<usize>) -> LogsResult {
	log_ring().lock().unwrap().snapshot(max)
}

/// Mirror logs to `agent.log` next to the config. A Windows scheduled task (and
/// detached service starts in general) has no console, so without this there is
/// nowhere to see why the agent failed to connect. Truncated on each start so it
/// can't grow without bound.
fn init_log_file(cfg_path: &Path) {
	let Some(dir) = cfg_path.parent() else { return };
	if let Ok(f) = OpenOptions::new()
		.create(true)
		.write(true)
		.truncate(true)
		.open(dir.join("agent.log"))
	{
		let _ = LOG_FILE.set(Mutex::new(f));
	}
}

/// Record an info-level line. Most call sites use this; reach for [`log_at`] only
/// where the line is a warning or an error worth filtering on in the Logs page.
pub fn log(msg: &str) {
	log_at(LogLevel::Info, msg);
}

/// Record a line at `level`: print it, mirror it to `agent.log`, and push it onto
/// the in-memory ring the controller can fetch.
pub fn log_at(level: LogLevel, msg: &str) {
	let line = format!("[libretether-agent] {msg}");
	eprintln!("{line}");
	if let Some(file) = LOG_FILE.get() {
		if let Ok(mut file) = file.lock() {
			let _ = writeln!(file, "{line}");
		}
	}
	log_ring().lock().unwrap().push(
		LogLine {
			ts_secs: host::now_secs(),
			level,
			message: msg.to_string(),
		},
		LOG_BUFFER_CAP,
	);
}

/// Load config and run the connect/serve loop until a shutdown signal arrives.
pub async fn run(cfg_path: PathBuf) -> Result<()> {
	init_log_file(&cfg_path);
	let mut cfg = AgentConfig::load(&cfg_path)?;
	handlers::mark_start();
	let target = match cfg.relay() {
		Some(relay) => format!("relay {relay}"),
		None => format!("controller {}", cfg.controller_addr),
	};
	log(&format!("agent {AGENT_VERSION} starting; {target}"));

	tokio::select! {
		_ = connect_loop(&mut cfg, &cfg_path) => {}
		_ = libretether_common::shutdown_signal() => log("shutdown signal received; exiting"),
	}
	Ok(())
}

/// Dial + serve forever, reconnecting with capped backoff.
async fn connect_loop(cfg: &mut AgentConfig, cfg_path: &Path) {
	let mut backoff = Backoff::new(RECONNECT_MAX_SECS);
	loop {
		// `connect_once` only returns once the link is gone (serve loops until the
		// connection ends), so time how long it lasted: a long, healthy session
		// resets the backoff; a fast failure keeps growing it.
		let started = Instant::now();
		if let Err(e) = connect_once(cfg, cfg_path).await {
			log_at(LogLevel::Error, &format!("connection error: {e:#}"));
		}
		if started.elapsed() >= Duration::from_secs(RECONNECT_STABLE_SECS) {
			backoff.reset();
		}
		let wait = backoff.next_delay();
		log(&format!("reconnecting in {}s", wait.as_secs()));
		tokio::time::sleep(wait).await;
	}
}

async fn connect_once(cfg: &mut AgentConfig, cfg_path: &Path) -> Result<()> {
	// Resolve the peer first so the client endpoint can match its address family.
	let (addr, is_relay) = match cfg.relay() {
		Some(relay) => (tls::resolve(relay).await?, true),
		None => (tls::resolve(&cfg.controller_addr).await?, false),
	};
	let endpoint = tls::client_endpoint(addr).context("binding client socket")?;
	let conn = if is_relay {
		log(&format!("dialing relay at {addr}"));
		connect_relay(&endpoint, addr, cfg).await?
	} else {
		log(&format!("dialing controller at {addr}"));
		dial(&endpoint, addr, &cfg.server_name).await?
	};
	serve(conn, cfg, cfg_path).await
}

/// Dial the relay and register as an agent; the controller's streams will then
/// arrive piped through it.
async fn connect_relay(endpoint: &Endpoint, addr: SocketAddr, cfg: &AgentConfig) -> Result<quinn::Connection> {
	let conn = dial(endpoint, addr, &cfg.server_name).await?;
	let identity = cfg.identity()?;
	// Shared client side of the relay handshake (secret + key-ownership proof). We
	// don't use the hello stream afterwards — the relay routes the controller's
	// streams on fresh ones — so the returned halves are dropped.
	client_handshake(
		&conn,
		RelayRole::Agent,
		cfg.relay_secret.as_deref().unwrap_or_default(),
		&identity,
	)
	.await
	.context("relay registration")?;
	log("registered with relay; awaiting controller");
	Ok(conn)
}

async fn dial(endpoint: &Endpoint, addr: SocketAddr, server_name: &str) -> Result<quinn::Connection> {
	let connecting = endpoint.connect(addr, server_name)?;
	tokio::time::timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), connecting)
		.await
		.map_err(|_| anyhow!("dial timed out after {CONNECT_TIMEOUT_SECS}s"))?
		.context("quic handshake")
}

/// Complete the controller handshake, then service control/session/tunnel
/// streams until the connection ends. In relay mode the controller's streams
/// arrive piped through the relay; the logic is identical.
async fn serve(conn: quinn::Connection, cfg: &mut AgentConfig, cfg_path: &Path) -> Result<()> {
	log("connected; awaiting challenge");

	// Handshake stream is opened by the controller.
	let (mut send, mut recv) = conn.accept_bi().await.context("accept handshake stream")?;
	match read_frame_capped::<_, StreamOpen>(&mut recv, MAX_CONTROL_FRAME).await? {
		StreamOpen::Handshake => {}
		other => return Err(anyhow!("expected handshake stream, got {other:?}")),
	}

	let identity = cfg.identity()?;
	let tokens: TokenSet = Arc::new(Mutex::new(HashSet::new()));

	// The controller key must have been pinned at enrollment. There is no
	// trust-on-first-use: an agent without a pinned key must be re-enrolled.
	let expected = cfg.require_controller_key()?;

	// Mutual handshake: prove our identity, verify the controller's against the
	// pinned key, and receive the capability token (issued into `tokens`) that
	// every later stream must carry.
	let client_id = verify_and_grant(
		&mut send,
		&mut recv,
		&identity,
		cfg.enrollment_token.clone(),
		&expected,
		&tokens,
	)
	.await?;
	log(&format!(
		"authenticated as client {}",
		client_id.clone().unwrap_or_default()
	));

	// Burn the one-time enrollment token now that we're enrolled.
	if cfg.enrollment_token.is_some() {
		cfg.enrollment_token = None;
		cfg.client_id = client_id;
		if let Err(e) = cfg.save(cfg_path) {
			log_at(
				LogLevel::Warn,
				&format!("warning: could not persist enrolled config: {e}"),
			);
		}
	}

	// Serve control + session streams until the connection ends. In relay mode
	// this connection is to the relay and outlives any single controller, so a
	// reconnecting controller opens a fresh handshake stream here — hand the
	// identity, pinned key and token set to each stream (see `reauth`).
	let identity = Arc::new(identity);
	let controller_key = Arc::new(expected);
	loop {
		let (send, recv) = conn.accept_bi().await.map_err(|e| anyhow!("connection ended: {e}"))?;
		tokio::spawn(handle_stream(
			send,
			recv,
			identity.clone(),
			controller_key.clone(),
			tokens.clone(),
		));
	}
}

/// The agent side of the mutual handshake on a handshake stream: prove our
/// identity over the controller's nonce, verify the controller's signature over
/// our nonce against the expected (pinned) key, and on success issue a fresh
/// per-connection capability token. Returns the controller-assigned `client_id`,
/// or an error if the controller is rejected or fails verification.
async fn verify_and_grant(
	send: &mut SendStream,
	recv: &mut RecvStream,
	identity: &Identity,
	enrollment_token: Option<String>,
	expected_key: &str,
	tokens: &Mutex<HashSet<String>>,
) -> Result<Option<String>> {
	let challenge: Challenge = read_frame_capped(recv, MAX_CONTROL_FRAME)
		.await
		.context("reading challenge")?;
	// Fail closed on a version skew, mirroring the controller's `Hello.protocol`
	// check — the controller and agents are released together, so a mismatch means
	// one end is stale and must be upgraded (no compatibility shims).
	if challenge.protocol != PROTOCOL_VERSION {
		return Err(anyhow!(
			"controller protocol v{} != agent protocol v{PROTOCOL_VERSION} — upgrade both ends",
			challenge.protocol
		));
	}
	let agent_nonce = crypto::random_nonce_b64();
	let hello = Hello {
		protocol: PROTOCOL_VERSION,
		enrollment_token,
		public_key: identity.public_b64(),
		signature: identity.sign_b64(challenge.nonce.as_bytes()),
		agent_nonce: agent_nonce.clone(),
		host: host::host_info(),
		agent_version: AGENT_VERSION.to_string(),
	};
	write_frame(send, &hello).await.context("sending hello")?;

	let ack: HelloAck = read_frame_capped(recv, MAX_CONTROL_FRAME)
		.await
		.context("reading ack")?;
	if !ack.accepted {
		return Err(anyhow!("controller rejected agent: {}", ack.reason.unwrap_or_default()));
	}

	// Authenticate the controller before trusting it with any stream: its key
	// must match the pinned one and its signature over our nonce must verify.
	let presented = challenge.controller_key.trim();
	if !crypto::ct_eq(expected_key, presented) {
		return Err(anyhow!(
			"controller key mismatch — refusing connection (possible impersonation)"
		));
	}
	if !crypto::verify_b64(presented, agent_nonce.as_bytes(), &ack.controller_sig) {
		return Err(anyhow!("controller identity signature invalid — refusing connection"));
	}

	// Hand the verified controller a capability token for this connection. Only the
	// most recent handshake's token is valid: in relay mode this one connection
	// outlives many controller sessions, so evict any prior token rather than
	// letting the set grow unbounded (a displaced controller's streams are dead).
	let token = crypto::random_nonce_b64();
	{
		let mut tokens = tokens.lock().unwrap();
		tokens.clear();
		tokens.insert(token.clone());
	}
	write_frame(send, &SessionGrant { token })
		.await
		.context("sending session grant")?;
	let _ = send.finish();
	Ok(ack.client_id)
}

async fn handle_stream(
	mut send: SendStream,
	mut recv: RecvStream,
	identity: Arc<Identity>,
	controller_key: Arc<String>,
	tokens: TokenSet,
) {
	let open = match read_frame_capped::<_, StreamOpen>(&mut recv, MAX_CONTROL_FRAME).await {
		Ok(o) => o,
		Err(_) => return,
	};
	// Handshake streams establish trust; every other stream must present the
	// capability token from a completed handshake. This is what stops a party
	// that can reach the agent (e.g. through the relay with only the owner
	// secret) but cannot complete the mutual handshake from issuing commands.
	if !matches!(open, StreamOpen::Handshake) && !authed(&mut recv, &tokens).await {
		return;
	}
	match open {
		StreamOpen::Control => {
			let req: ControlRequest = match read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await {
				Ok(r) => r,
				Err(_) => return,
			};
			let resp = handlers::handle(req).await;
			let _ = write_frame(&mut send, &resp).await;
			let _ = send.finish();
		}
		StreamOpen::Session => {
			if let Err(e) = session::run(send, recv).await {
				log(&format!("session ended: {e}"));
			}
		}
		StreamOpen::Tunnel { port } => tunnel(port, send, recv).await,
		StreamOpen::Handshake => reauth(send, recv, &identity, &controller_key, &tokens).await,
	}
}

/// Read and check the capability token that prefixes a non-handshake stream.
async fn authed(recv: &mut RecvStream, tokens: &Mutex<HashSet<String>>) -> bool {
	let auth: StreamAuth = match read_frame_capped(recv, MAX_CONTROL_FRAME).await {
		Ok(a) => a,
		Err(_) => return false,
	};
	let ok = tokens.lock().unwrap().contains(&auth.token);
	if !ok {
		log_at(LogLevel::Warn, "rejected stream: missing or invalid capability token");
	}
	ok
}

/// Answer a fresh handshake on an already-serving connection. The relay keeps the
/// agent connected across controller restarts/reconnects, so a returning
/// controller re-authenticates by opening a new handshake stream; the one-time
/// token is long spent, so we identify by key and verify the controller against
/// the pinned key. A successful re-auth issues a new capability token.
async fn reauth(
	mut send: SendStream,
	mut recv: RecvStream,
	identity: &Identity,
	controller_key: &str,
	tokens: &Mutex<HashSet<String>>,
) {
	match verify_and_grant(&mut send, &mut recv, identity, None, controller_key, tokens).await {
		Ok(client_id) => log(&format!("re-authenticated as client {}", client_id.unwrap_or_default())),
		Err(e) => log_at(LogLevel::Warn, &format!("controller re-auth rejected: {e:#}")),
	}
}

/// Pipe a QUIC stream to a local TCP port (the client's RDP/SSH server) — used
/// to reach the client through the relay.
///
/// `port` is chosen by the controller and is not restricted to RDP/SSH: an
/// authenticated controller can reach any loopback service on the agent host.
/// This is within the trust model — only a controller that completed the mutual
/// handshake holds a capability token, and `handle_stream` rejects any tunnel
/// stream without one — but note the reach is intentionally unrestricted.
async fn tunnel(port: u16, mut q_send: SendStream, q_recv: RecvStream) {
	match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
		Ok(tcp) => {
			let (tcp_read, tcp_write) = tcp.into_split();
			pipe_bidirectional(q_recv, q_send, tcp_read, tcp_write).await;
		}
		Err(e) => {
			log_at(LogLevel::Warn, &format!("tunnel to 127.0.0.1:{port} failed: {e}"));
			let _ = q_send.finish();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::net::Ipv4Addr;

	fn line(ts: u64, msg: &str) -> LogLine {
		LogLine {
			ts_secs: ts,
			level: LogLevel::Info,
			message: msg.into(),
		}
	}

	// The ring keeps the most recent `cap` lines and flags `dropped` once it has
	// evicted anything; `snapshot(max)` returns the newest `max`, oldest first.
	#[test]
	fn log_ring_caps_evicts_oldest_and_snapshots_recent() {
		let mut ring = LogRing::default();
		for i in 0..5 {
			ring.push(line(i, &format!("m{i}")), 3);
		}
		// Cap 3 → only the last three survive, and an eviction happened.
		let all = ring.snapshot(None);
		assert!(all.dropped);
		assert_eq!(
			all.lines.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(),
			["m2", "m3", "m4"],
		);
		// `max` returns the newest N, still oldest-first.
		let tail = ring.snapshot(Some(2));
		assert_eq!(
			tail.lines.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(),
			["m3", "m4"],
		);
		// A fresh ring reports nothing dropped.
		assert!(!LogRing::default().snapshot(None).dropped);
	}

	/// A connected QUIC pair over loopback using the project's real transport
	/// config. Returns `(controller_endpoint, controller_conn, agent_endpoint,
	/// agent_conn)`. The endpoints must be kept alive for the connections to work,
	/// so callers bind them (even as `_`-prefixed) for the test's duration.
	///
	/// The controller is the QUIC *server* (it opens streams); the agent is the
	/// client (it accepts) — exactly as in direct mode.
	async fn loopback() -> (Endpoint, quinn::Connection, Endpoint, quinn::Connection) {
		tls::install_crypto_provider();
		let (cert, key) = tls::self_signed();
		let server_ep = Endpoint::server(tls::server_config(cert, key), (Ipv4Addr::LOCALHOST, 0).into()).unwrap();
		let addr = server_ep.local_addr().unwrap();
		let client_ep = tls::client_endpoint(addr).unwrap();
		let accept = {
			let ep = server_ep.clone();
			tokio::spawn(async move { ep.accept().await.unwrap().accept().unwrap().await.unwrap() })
		};
		let client_conn = client_ep.connect(addr, "libretether.local").unwrap().await.unwrap();
		let server_conn = accept.await.unwrap();
		(server_ep, server_conn, client_ep, client_conn)
	}

	/// What `verify_and_grant` resolves to: the `client_id` on success.
	type VerifyOutcome = Result<Option<String>>;

	/// Run the agent's real `verify_and_grant` on the agent (client) side.
	fn spawn_agent(
		agent_conn: quinn::Connection,
		agent: Identity,
		pinned_controller_key: String,
		tokens: TokenSet,
	) -> tokio::task::JoinHandle<VerifyOutcome> {
		tokio::spawn(async move {
			let (mut send, mut recv) = agent_conn.accept_bi().await.map_err(|e| anyhow!("accept: {e}"))?;
			verify_and_grant(&mut send, &mut recv, &agent, None, &pinned_controller_key, &tokens).await
		})
	}

	#[tokio::test]
	async fn mutual_handshake_succeeds_and_issues_a_capability_token() {
		let (_sep, server, _cep, client) = loopback().await;
		let ctrl = Identity::generate();
		let agent = Identity::generate();
		let tokens: TokenSet = Arc::new(Mutex::new(HashSet::new()));

		// Pass a cloned handle so dropping the agent task at the end of the handshake
		// doesn't close the connection out from under the controller's final read
		// (in production `serve` holds the connection open in a loop).
		let handle = spawn_agent(client.clone(), agent, ctrl.public_b64(), tokens.clone());

		// An honest controller: presents its real key, verifies the agent's
		// signature, and signs the agent's nonce with its identity key.
		let (mut c_send, mut c_recv) = server.open_bi().await.unwrap();
		let nonce = crypto::random_nonce_b64();
		write_frame(
			&mut c_send,
			&Challenge {
				protocol: PROTOCOL_VERSION,
				nonce: nonce.clone(),
				controller_key: ctrl.public_b64(),
			},
		)
		.await
		.unwrap();
		let hello: Hello = read_frame_capped(&mut c_recv, MAX_CONTROL_FRAME).await.unwrap();
		assert_eq!(hello.protocol, PROTOCOL_VERSION);
		assert!(crypto::verify_b64(
			&hello.public_key,
			nonce.as_bytes(),
			&hello.signature
		));
		write_frame(
			&mut c_send,
			&HelloAck {
				accepted: true,
				reason: None,
				client_id: Some("cid-1".into()),
				controller_sig: ctrl.sign_b64(hello.agent_nonce.as_bytes()),
			},
		)
		.await
		.unwrap();
		let grant: SessionGrant = read_frame_capped(&mut c_recv, MAX_CONTROL_FRAME).await.unwrap();

		let client_id = handle.await.unwrap().expect("handshake should succeed");
		assert_eq!(client_id.as_deref(), Some("cid-1"));
		// The token the agent issued (and stored) is exactly the one the controller
		// received — and the only one the agent will later honour.
		assert!(tokens.lock().unwrap().contains(&grant.token));
		assert_eq!(tokens.lock().unwrap().len(), 1);
	}

	/// Drive the controller side up to (and including) the `HelloAck`, with knobs
	/// to forge the presented key / the signing key / the verdict. Used by the
	/// rejection tests — the agent errors out before reading a `SessionGrant`.
	async fn drive_dishonest_controller(
		server: &quinn::Connection,
		presented_key: String,
		signer: &Identity,
		accepted: bool,
	) {
		drive_controller_with_protocol(server, PROTOCOL_VERSION, presented_key, signer, accepted).await
	}

	/// Like [`drive_dishonest_controller`] but with an explicit protocol version,
	/// so tests can forge a version skew.
	async fn drive_controller_with_protocol(
		server: &quinn::Connection,
		protocol: u32,
		presented_key: String,
		signer: &Identity,
		accepted: bool,
	) {
		let (mut c_send, mut c_recv) = server.open_bi().await.unwrap();
		let nonce = crypto::random_nonce_b64();
		write_frame(
			&mut c_send,
			&Challenge {
				protocol,
				nonce,
				controller_key: presented_key,
			},
		)
		.await
		.unwrap();
		let hello: Hello = read_frame_capped(&mut c_recv, MAX_CONTROL_FRAME).await.unwrap();
		write_frame(
			&mut c_send,
			&HelloAck {
				accepted,
				reason: (!accepted).then(|| "rejected".to_string()),
				client_id: None,
				controller_sig: signer.sign_b64(hello.agent_nonce.as_bytes()),
			},
		)
		.await
		.unwrap();
		let _ = c_send.finish();
	}

	#[tokio::test]
	async fn rejects_a_controller_key_that_does_not_match_the_pinned_one() {
		let (_sep, server, _cep, client) = loopback().await;
		let pinned = Identity::generate();
		let imposter = Identity::generate();
		let tokens: TokenSet = Arc::new(Mutex::new(HashSet::new()));

		let handle = spawn_agent(client, Identity::generate(), pinned.public_b64(), tokens.clone());
		// Imposter presents its own key (and signs with it) — a different key than
		// the agent pinned at enrollment.
		drive_dishonest_controller(&server, imposter.public_b64(), &imposter, true).await;

		assert!(handle.await.unwrap().is_err());
		assert!(
			tokens.lock().unwrap().is_empty(),
			"no token issued to an unverified controller"
		);
	}

	#[tokio::test]
	async fn rejects_a_controller_protocol_version_mismatch() {
		let (_sep, server, _cep, client) = loopback().await;
		let ctrl = Identity::generate();
		let tokens: TokenSet = Arc::new(Mutex::new(HashSet::new()));

		let handle = spawn_agent(client, Identity::generate(), ctrl.public_b64(), tokens.clone());
		// Honest key, but a stale protocol version: the agent must fail closed right
		// after reading the challenge — before it even sends its Hello — so we only
		// send the challenge here (there is no Hello to read back).
		let (mut c_send, _c_recv) = server.open_bi().await.unwrap();
		write_frame(
			&mut c_send,
			&Challenge {
				protocol: PROTOCOL_VERSION + 1,
				nonce: crypto::random_nonce_b64(),
				controller_key: ctrl.public_b64(),
			},
		)
		.await
		.unwrap();

		assert!(handle.await.unwrap().is_err());
		assert!(
			tokens.lock().unwrap().is_empty(),
			"no token issued to a version-mismatched controller"
		);
	}

	#[tokio::test]
	async fn rejects_an_empty_controller_key_downgrade() {
		let (_sep, server, _cep, client) = loopback().await;
		let pinned = Identity::generate();
		let tokens: TokenSet = Arc::new(Mutex::new(HashSet::new()));

		let handle = spawn_agent(client, Identity::generate(), pinned.public_b64(), tokens.clone());
		// A peer that omits/blanks the controller key (the downgrade the
		// `#[serde(default)]` removal guards against) must be rejected.
		drive_dishonest_controller(&server, String::new(), &pinned, true).await;

		assert!(handle.await.unwrap().is_err());
		assert!(tokens.lock().unwrap().is_empty());
	}

	#[tokio::test]
	async fn rejects_a_bad_controller_signature() {
		let (_sep, server, _cep, client) = loopback().await;
		let ctrl = Identity::generate();
		let wrong = Identity::generate();
		let tokens: TokenSet = Arc::new(Mutex::new(HashSet::new()));

		let handle = spawn_agent(client, Identity::generate(), ctrl.public_b64(), tokens.clone());
		// Correct key is presented, but the nonce is signed by the wrong key, so
		// the signature won't verify against the pinned key.
		drive_dishonest_controller(&server, ctrl.public_b64(), &wrong, true).await;

		assert!(handle.await.unwrap().is_err());
		assert!(tokens.lock().unwrap().is_empty());
	}

	#[tokio::test]
	async fn rejects_when_the_controller_declines_the_agent() {
		let (_sep, server, _cep, client) = loopback().await;
		let ctrl = Identity::generate();
		let tokens: TokenSet = Arc::new(Mutex::new(HashSet::new()));

		let handle = spawn_agent(client, Identity::generate(), ctrl.public_b64(), tokens.clone());
		drive_dishonest_controller(&server, ctrl.public_b64(), &ctrl, false).await;

		assert!(handle.await.unwrap().is_err());
		assert!(tokens.lock().unwrap().is_empty());
	}

	#[tokio::test]
	async fn authed_accepts_only_known_capability_tokens() {
		let (_sep, server, _cep, client) = loopback().await;
		let tokens: TokenSet = Arc::new(Mutex::new(HashSet::new()));
		tokens.lock().unwrap().insert("good-token".to_string());

		// A stream stamped with the issued token passes.
		let writer = {
			let server = server.clone();
			tokio::spawn(async move {
				let (mut s, _r) = server.open_bi().await.unwrap();
				write_frame(
					&mut s,
					&StreamAuth {
						token: "good-token".into(),
					},
				)
				.await
				.unwrap();
				let _ = s.finish();
			})
		};
		let (_s, mut recv) = client.accept_bi().await.unwrap();
		assert!(authed(&mut recv, &tokens).await);
		writer.await.unwrap();

		// A stream with an unknown token is rejected.
		let writer = {
			let server = server.clone();
			tokio::spawn(async move {
				let (mut s, _r) = server.open_bi().await.unwrap();
				write_frame(&mut s, &StreamAuth { token: "forged".into() })
					.await
					.unwrap();
				let _ = s.finish();
			})
		};
		let (_s, mut recv) = client.accept_bi().await.unwrap();
		assert!(!authed(&mut recv, &tokens).await);
		writer.await.unwrap();
	}

	#[tokio::test]
	async fn tunnel_pipes_bytes_to_a_local_tcp_port_both_ways() {
		// A loopback echo server the tunnel connects to (the stand-in RDP/SSH port).
		let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
		let port = listener.local_addr().unwrap().port();
		tokio::spawn(async move {
			if let Ok((mut sock, _)) = listener.accept().await {
				let mut buf = [0u8; 64];
				let n = sock.read(&mut buf).await.unwrap_or(0);
				let _ = sock.write_all(&buf[..n]).await;
				let _ = sock.flush().await;
			}
		});

		let (_sep, server, _cep, client) = loopback().await;
		// Keep `client` alive in the test; the task gets a clone so finishing the
		// tunnel doesn't tear the connection down before the controller's read.
		let agent_conn = client.clone();
		let agent = tokio::spawn(async move {
			let (send, recv) = agent_conn.accept_bi().await.unwrap();
			tunnel(port, send, recv).await;
		});

		let (mut send, mut recv) = server.open_bi().await.unwrap();
		send.write_all(b"hello-tunnel").await.unwrap();
		let _ = send.finish();
		let echoed = recv.read_to_end(64).await.unwrap();
		assert_eq!(echoed, b"hello-tunnel");
		agent.await.unwrap();
	}

	use tokio::io::{AsyncReadExt, AsyncWriteExt};
}
