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
use libretether_protocol::e2e::{self, SecureQuicRecv, SecureQuicSend, SessionKey};
use libretether_protocol::frame::{read_frame_capped, write_frame, MAX_CONTROL_FRAME};
use libretether_protocol::pairing::{agent_pair, PairBundle, PairingCode};
use libretether_protocol::relay::{client_handshake, pairing_join, RelayRole, RelaySignal};
use libretether_protocol::{
	tls, Challenge, ControlRequest, Hello, HelloAck, LogLevel, LogLine, LogsResult, SessionGrant, StreamAuth,
	StreamOpen, PROTOCOL_VERSION,
};
use quinn::{Endpoint, RecvStream, SendStream};
use tokio::io::AsyncWriteExt;

/// Shared set of capability tokens issued (one per completed handshake) over a
/// single connection's lifetime; a control/session/tunnel stream must present one.
type TokenSet = Arc<Mutex<HashSet<String>>>;

/// The end-to-end session key from the most recent completed handshake. Every
/// post-handshake stream is AEAD-sealed under a key derived from this, so a relay
/// forwarding the bytes only ever sees ciphertext. `None` until the first handshake;
/// replaced on each re-auth (in relay mode this connection outlives many controller
/// sessions), mirroring the single-valid-token rule in [`verify_and_grant`].
type SessionSlot = Arc<Mutex<Option<SessionKey>>>;

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
	/// Count of all lines ever recorded (including evicted ones), reported as
	/// [`LogsResult::next_seq`] so a consumer could fetch incrementally.
	next_seq: u64,
}

impl LogRing {
	/// Append `line`, evicting the oldest and flagging `dropped` once `cap` is reached.
	fn push(&mut self, line: LogLine, cap: usize) {
		if self.lines.len() >= cap {
			self.lines.pop_front();
			self.dropped = true;
		}
		self.lines.push_back(line);
		self.next_seq += 1;
	}

	/// The most recent `max` lines (all when `None`), oldest first. Stamps the
	/// agent's current clock so the controller can re-anchor the line timestamps to
	/// its own (the agent may be in another timezone or have a skewed clock).
	fn snapshot(&self, max: Option<usize>) -> LogsResult {
		let take = max.unwrap_or(self.lines.len()).min(self.lines.len());
		LogsResult {
			lines: self.lines.iter().skip(self.lines.len() - take).cloned().collect(),
			dropped: self.dropped,
			now_secs: host::now_secs(),
			next_seq: self.next_seq,
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

/// Record a debug-level line: fine-grained, chatty per-stream/per-step detail
/// (stream open/close, handshake sub-steps, per-second stream stats) that the
/// Logs page can filter out. Reach for [`log`] for connection-level milestones.
pub fn debug(msg: &str) {
	log_at(LogLevel::Debug, msg);
}

/// Record a line at `level`: print it, mirror it to `agent.log`, and push it onto
/// the in-memory ring the controller can fetch.
pub fn log_at(level: LogLevel, msg: &str) {
	let line = format!("[libretether-agent] {msg}");
	// Write to stderr *fallibly*. A service launched with no console — the Windows
	// HKCU Run entry / scheduled task, where the windowless (GUI-subsystem) binary
	// has an invalid stderr handle — makes `eprintln!` PANIC on the failed write,
	// which under `panic = "abort"` would kill the agent the instant it logged its
	// first line (leaving an empty `agent.log`). A missing console is fine to ignore.
	let _ = writeln!(std::io::stderr(), "{line}");
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
	if is_relay {
		log(&format!("dialing relay at {addr}"));
		let (conn, hello_send, hello_recv) = connect_relay(&endpoint, addr, cfg).await?;
		debug(&format!("quic connection established to {addr}"));
		// Watch for peer-to-peer punch signals from the relay and, when one arrives,
		// dial the controller directly from this same socket (see `handle_signals`).
		// Reads the pinned key/identity now, before `serve` borrows `cfg` mutably.
		let identity = Arc::new(cfg.identity()?);
		let controller_key = Arc::new(cfg.require_controller_key()?);
		let server_name = cfg.server_name.clone();
		let signals = tokio::spawn(handle_signals(
			endpoint.clone(),
			hello_send,
			hello_recv,
			identity,
			controller_key,
			server_name,
		));
		// Serving the relay connection blocks until it drops; then stop the signal
		// watcher (a reconnect starts a fresh one) and any direct paths tear down with it.
		let result = serve(conn, cfg, cfg_path).await;
		signals.abort();
		result
	} else {
		log(&format!("dialing controller at {addr}"));
		let conn = dial(&endpoint, addr, &cfg.server_name).await?;
		debug(&format!("quic connection established to {addr}"));
		serve(conn, cfg, cfg_path).await
	}
}

/// Dial the relay and register as an agent; the controller's streams will then
/// arrive piped through it. Returns the connection plus the hello stream's halves —
/// the relay pushes peer-to-peer punch signals on that stream, which the agent reads
/// (`hello_recv`) while holding `hello_send` open to keep the stream alive.
async fn connect_relay(
	endpoint: &Endpoint,
	addr: SocketAddr,
	cfg: &AgentConfig,
) -> Result<(quinn::Connection, SendStream, RecvStream)> {
	let conn = dial(endpoint, addr, &cfg.server_name).await?;
	let identity = cfg.identity()?;
	// Shared client side of the relay handshake (secret + key-ownership proof).
	let (hello_send, hello_recv) = client_handshake(
		&conn,
		RelayRole::Agent,
		cfg.relay_secret.as_deref().unwrap_or_default(),
		&identity,
	)
	.await
	.context("relay registration")?;
	log("registered with relay; awaiting controller");
	Ok((conn, hello_send, hello_recv))
}

/// Read peer-to-peer signals the relay pushes on the agent's hello stream and act on
/// them. Today the only signal is [`RelaySignal::Punch`]: the agent dials the
/// controller's reflexive address directly, from the *same* endpoint/socket it uses
/// for the relay, so the NAT mapping the relay observed is reused and the punch can
/// land. `hello_send` is held only to keep the stream open.
async fn handle_signals(
	endpoint: Endpoint,
	_hello_send: SendStream,
	mut hello_recv: RecvStream,
	identity: Arc<Identity>,
	controller_key: Arc<String>,
	server_name: String,
) {
	loop {
		let signal = match read_frame_capped::<_, RelaySignal>(&mut hello_recv, MAX_CONTROL_FRAME).await {
			Ok(s) => s,
			Err(_) => break, // the relay closed the signal channel (session ended)
		};
		match signal {
			RelaySignal::Punch {
				controller_addr,
				rendezvous,
			} => {
				let endpoint = endpoint.clone();
				let identity = identity.clone();
				let controller_key = controller_key.clone();
				let server_name = server_name.clone();
				tokio::spawn(async move {
					punch_to_controller(
						&endpoint,
						&controller_addr,
						&rendezvous,
						identity,
						controller_key,
						&server_name,
					)
					.await;
				});
			}
		}
	}
}

/// Attempt a direct peer-to-peer connection to the controller for a hole-punch. The
/// controller address is chosen by the relay, so it is *not* trusted here — the direct
/// connection still runs the full mutual handshake (`serve_direct`), so a bogus or
/// hostile address simply fails to authenticate and is dropped (fail-closed). On a
/// successful punch the agent serves the controller over the direct path until it
/// drops; on failure (typically symmetric NAT/CGNAT) it stays on the relay.
async fn punch_to_controller(
	endpoint: &Endpoint,
	controller_addr: &str,
	rendezvous: &str,
	identity: Arc<Identity>,
	controller_key: Arc<String>,
	server_name: &str,
) {
	let Ok(addr) = controller_addr.parse::<SocketAddr>() else {
		log_at(
			LogLevel::Warn,
			&format!("ignoring punch signal with an unparseable controller address {controller_addr:?}"),
		);
		return;
	};
	let rv = rendezvous.chars().take(8).collect::<String>();
	debug(&format!(
		"hole-punch: dialing controller directly at {addr} (rendezvous {rv}…)"
	));
	// Dial from the relay socket. quinn retransmits Initials across the connect window,
	// covering the brief moment before the controller punches its own NAT open.
	match dial(endpoint, addr, server_name).await {
		Ok(conn) => {
			log(&format!("direct peer-to-peer path to controller {addr} established"));
			if let Err(e) = serve_direct(conn, identity, controller_key).await {
				debug(&format!("direct path to {addr} ended: {e}"));
			}
		}
		Err(e) => debug(&format!(
			"hole-punch to {addr} did not connect ({e}); staying on the relay"
		)),
	}
}

async fn dial(endpoint: &Endpoint, addr: SocketAddr, server_name: &str) -> Result<quinn::Connection> {
	let connecting = endpoint.connect(addr, server_name)?;
	tokio::time::timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), connecting)
		.await
		.map_err(|_| anyhow!("dial timed out after {CONNECT_TIMEOUT_SECS}s"))?
		.context("quic handshake")
}

/// One-shot pairing for the `pair` subcommand: dial the relay, join the pairing
/// mailbox under the code's nameplate, and run the agent side of the PAKE. Returns
/// the enrollment [`PairBundle`] and the verify phrase to show the operator. No
/// secret or identity is trusted by the relay — the PAKE is the authentication, and
/// a wrong code (or any tampering) makes this fail closed.
pub async fn pair(relay_addr: &str, code: &PairingCode, server_name: &str) -> Result<(PairBundle, String)> {
	let addr = tls::resolve(relay_addr).await.context("resolving the relay address")?;
	let endpoint = tls::client_endpoint(addr).context("binding client socket")?;
	let conn = dial(&endpoint, addr, server_name).await.context("dialing the relay")?;
	let (mut send, mut recv) = pairing_join(&conn, &code.nameplate)
		.await
		.context("joining the pairing mailbox")?;
	agent_pair(&mut send, &mut recv, code)
		.await
		.context("pairing handshake (wrong code, or the slot expired)")
}

/// Complete the controller handshake, then service control/session/tunnel
/// streams until the connection ends. In relay mode the controller's streams
/// arrive piped through the relay; the logic is identical.
async fn serve(conn: quinn::Connection, cfg: &mut AgentConfig, cfg_path: &Path) -> Result<()> {
	log("connected; awaiting challenge");
	let identity = cfg.identity()?;
	// The controller key must have been pinned at enrollment. There is no
	// trust-on-first-use: an agent without a pinned key must be re-enrolled.
	let expected = cfg.require_controller_key()?;

	// Accept the controller's handshake stream and complete the mutual auth + ECDHE.
	let (client_id, tokens, session_key) =
		accept_handshake(&conn, &identity, cfg.enrollment_token.clone(), &expected).await?;
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

	serve_streams(conn, Arc::new(identity), Arc::new(expected), tokens, session_key).await
}

/// Serve one already-established direct peer-to-peer connection (a NAT hole-punch
/// upgrade). Identical to [`serve`] minus the config side (the agent is already
/// enrolled, so there is no enrollment token to burn or config to persist).
async fn serve_direct(conn: quinn::Connection, identity: Arc<Identity>, controller_key: Arc<String>) -> Result<()> {
	let (client_id, tokens, session_key) = accept_handshake(&conn, &identity, None, &controller_key).await?;
	log(&format!(
		"direct peer-to-peer path up as client {}",
		client_id.unwrap_or_default()
	));
	serve_streams(conn, identity, controller_key, tokens, session_key).await
}

/// Accept the controller's handshake stream (the first stream it opens) and run the
/// mutual auth + end-to-end key agreement, returning the client id and the populated
/// token/session-key slots the serve loop hands to each subsequent stream.
async fn accept_handshake(
	conn: &quinn::Connection,
	identity: &Identity,
	enrollment_token: Option<String>,
	controller_key: &str,
) -> Result<(Option<String>, TokenSet, SessionSlot)> {
	let (mut send, mut recv) = conn.accept_bi().await.context("accept handshake stream")?;
	debug("handshake stream accepted; verifying controller");
	match read_frame_capped::<_, StreamOpen>(&mut recv, MAX_CONTROL_FRAME).await? {
		StreamOpen::Handshake => {}
		other => return Err(anyhow!("expected handshake stream, got {other:?}")),
	}
	let tokens: TokenSet = Arc::new(Mutex::new(HashSet::new()));
	// The end-to-end key agreed by the handshake; every later stream is sealed under it.
	let session_key: SessionSlot = Arc::new(Mutex::new(None));
	let client_id = verify_and_grant(
		&mut send,
		&mut recv,
		identity,
		enrollment_token,
		controller_key,
		&tokens,
		&session_key,
	)
	.await?;
	Ok((client_id, tokens, session_key))
}

/// Serve control/session/tunnel streams on `conn` until it ends. In relay mode this
/// connection is to the relay and outlives any single controller, so a reconnecting
/// controller opens a fresh handshake stream here (see `reauth`), replacing the token
/// and session key in the shared slots.
async fn serve_streams(
	conn: quinn::Connection,
	identity: Arc<Identity>,
	controller_key: Arc<String>,
	tokens: TokenSet,
	session_key: SessionSlot,
) -> Result<()> {
	loop {
		let (send, recv) = conn.accept_bi().await.map_err(|e| anyhow!("connection ended: {e}"))?;
		debug("accepted a new stream from the controller");
		tokio::spawn(handle_stream(
			send,
			recv,
			identity.clone(),
			controller_key.clone(),
			tokens.clone(),
			session_key.clone(),
		));
	}
}

/// A short label for a stream type, for the debug logs in [`handle_stream`].
fn stream_label(open: &StreamOpen) -> String {
	match open {
		StreamOpen::Handshake => "handshake".into(),
		StreamOpen::Control => "control".into(),
		StreamOpen::Session => "session".into(),
		StreamOpen::Tunnel { port } => format!("tunnel→127.0.0.1:{port}"),
		StreamOpen::Download => "download".into(),
		StreamOpen::Upload => "upload".into(),
	}
}

/// The agent side of the mutual handshake on a handshake stream: agree an ephemeral
/// key, prove our identity by signing the shared transcript, verify the
/// controller's signature over the same transcript against the expected (pinned)
/// key, and on success derive the end-to-end session key and issue a fresh
/// per-connection capability token. Returns the controller-assigned `client_id`, or
/// an error if the controller is rejected or fails verification.
async fn verify_and_grant(
	send: &mut SendStream,
	recv: &mut RecvStream,
	identity: &Identity,
	enrollment_token: Option<String>,
	expected_key: &str,
	tokens: &Mutex<HashSet<String>>,
	session_key: &Mutex<Option<SessionKey>>,
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
	let controller_eph =
		e2e::decode_eph(&challenge.controller_eph).ok_or_else(|| anyhow!("controller ephemeral key is malformed"))?;

	// Our ephemeral half of the end-to-end key agreement, and the transcript both
	// ends sign: it commits to both ephemeral keys and both nonces, so a relay can't
	// swap in its own ephemeral key without breaking a signature it can't forge.
	let ephemeral = e2e::EphemeralKeypair::generate();
	let agent_nonce = crypto::random_nonce_b64();
	let transcript = e2e::handshake_transcript(
		&controller_eph,
		&ephemeral.public_bytes(),
		&challenge.nonce,
		&agent_nonce,
	);
	let hello = Hello {
		protocol: PROTOCOL_VERSION,
		enrollment_token,
		public_key: identity.public_b64(),
		signature: identity.sign_b64(&transcript),
		agent_nonce: agent_nonce.clone(),
		agent_eph: ephemeral.public_b64(),
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
	// must match the pinned one and its signature over the transcript must verify.
	let presented = challenge.controller_key.trim();
	if !crypto::ct_eq(expected_key, presented) {
		return Err(anyhow!(
			"controller key mismatch — refusing connection (possible impersonation)"
		));
	}
	if !crypto::verify_b64(presented, &transcript, &ack.controller_sig) {
		return Err(anyhow!("controller identity signature invalid — refusing connection"));
	}

	// The controller is verified and its ephemeral key is bound to that verified
	// signature, so complete the ECDHE and derive the end-to-end session key.
	let shared = ephemeral
		.diffie_hellman(&controller_eph)
		.ok_or_else(|| anyhow!("end-to-end key agreement produced a degenerate shared secret"))?;
	let key = SessionKey::derive(&shared, &transcript);

	// Hand the verified controller a capability token for this connection, and store
	// the session key it will use. Only the most recent handshake's token/key is
	// valid: in relay mode this one connection outlives many controller sessions, so
	// replace any prior state (a displaced controller's streams are dead). Store both
	// before sending the grant, so the controller's first encrypted stream can never
	// arrive before we're ready to decrypt it.
	let token = crypto::random_nonce_b64();
	*session_key.lock().unwrap() = Some(key);
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
	send: SendStream,
	mut recv: RecvStream,
	identity: Arc<Identity>,
	controller_key: Arc<String>,
	tokens: TokenSet,
	session_key: SessionSlot,
) {
	let open = match read_frame_capped::<_, StreamOpen>(&mut recv, MAX_CONTROL_FRAME).await {
		Ok(o) => o,
		Err(_) => return,
	};
	debug(&format!("stream opened: {}", stream_label(&open)));

	// The handshake stream establishes trust and the end-to-end key, so it stays
	// plaintext (there's no key yet). Every other stream is sealed under the session
	// key from a completed handshake — the controller sends a per-stream salt, then
	// the capability token and payload flow as AEAD records, so a relay forwarding
	// the bytes only sees ciphertext.
	if matches!(open, StreamOpen::Handshake) {
		reauth(send, recv, &identity, &controller_key, &tokens, &session_key).await;
		return;
	}
	let Some(key) = session_key.lock().unwrap().clone() else {
		// A non-handshake stream before any handshake completed (only reachable if a
		// relay routes one) has no key to decrypt under — drop it, fail closed.
		log_at(LogLevel::Warn, "rejected stream: no end-to-end session established");
		return;
	};
	let (mut send, mut recv) = match e2e::open_secure_agent(send, recv, &key).await {
		Ok(pair) => pair,
		Err(_) => return,
	};
	// The capability token now arrives over the encrypted channel. A valid decrypt
	// already proves the peer completed the handshake; the token check is the
	// belt-and-suspenders early reject it has always been.
	if !authed(&mut recv, &tokens).await {
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
			let _ = send.shutdown().await;
			debug("control stream closed");
		}
		StreamOpen::Session => {
			debug("session stream opened");
			if let Err(e) = session::run(send, recv).await {
				log(&format!("session ended: {e}"));
			}
			debug("session stream closed");
		}
		StreamOpen::Tunnel { port } => tunnel(port, send, recv).await,
		StreamOpen::Download => {
			debug("download stream opened");
			crate::transfer::serve_download(send, recv).await;
			debug("download stream closed");
		}
		StreamOpen::Upload => {
			debug("upload stream opened");
			crate::transfer::serve_upload(send, recv).await;
			debug("upload stream closed");
		}
		// Handled above (plaintext); unreachable here.
		StreamOpen::Handshake => {}
	}
}

/// Read and check the capability token that prefixes a non-handshake stream (read
/// through the encrypted channel).
async fn authed<R: tokio::io::AsyncRead + Unpin>(recv: &mut R, tokens: &Mutex<HashSet<String>>) -> bool {
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
/// the pinned key. A successful re-auth issues a new capability token and a fresh
/// end-to-end session key.
async fn reauth(
	mut send: SendStream,
	mut recv: RecvStream,
	identity: &Identity,
	controller_key: &str,
	tokens: &Mutex<HashSet<String>>,
	session_key: &Mutex<Option<SessionKey>>,
) {
	match verify_and_grant(
		&mut send,
		&mut recv,
		identity,
		None,
		controller_key,
		tokens,
		session_key,
	)
	.await
	{
		Ok(client_id) => log(&format!("re-authenticated as client {}", client_id.unwrap_or_default())),
		Err(e) => log_at(LogLevel::Warn, &format!("controller re-auth rejected: {e:#}")),
	}
}

/// Pipe an end-to-end-encrypted QUIC stream to a local TCP port (the client's
/// RDP/SSH server) — used to reach the client through the relay.
///
/// `port` is chosen by the controller and is not restricted to RDP/SSH: an
/// authenticated controller can reach any loopback service on the agent host.
/// This is within the trust model — only a controller that completed the mutual
/// handshake holds the session key and capability token, and `handle_stream`
/// rejects any tunnel stream without them — but note the reach is intentionally
/// unrestricted.
async fn tunnel(port: u16, mut q_send: SecureQuicSend, q_recv: SecureQuicRecv) {
	debug(&format!("tunnel: connecting to 127.0.0.1:{port}"));
	match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
		Ok(tcp) => {
			debug(&format!("tunnel: connected to 127.0.0.1:{port}, piping"));
			let (tcp_read, tcp_write) = tcp.into_split();
			pipe_bidirectional(q_recv, q_send, tcp_read, tcp_write).await;
			debug(&format!("tunnel: 127.0.0.1:{port} closed"));
		}
		Err(e) => {
			log_at(LogLevel::Warn, &format!("tunnel to 127.0.0.1:{port} failed: {e}"));
			let _ = q_send.shutdown().await;
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

	/// Run the agent's real `verify_and_grant` on the agent (client) side, storing
	/// the agreed session key into `session_key` so a test can confirm the
	/// end-to-end key was (or wasn't) established.
	fn spawn_agent(
		agent_conn: quinn::Connection,
		agent: Identity,
		pinned_controller_key: String,
		tokens: TokenSet,
		session_key: SessionSlot,
	) -> tokio::task::JoinHandle<VerifyOutcome> {
		tokio::spawn(async move {
			let (mut send, mut recv) = agent_conn.accept_bi().await.map_err(|e| anyhow!("accept: {e}"))?;
			verify_and_grant(
				&mut send,
				&mut recv,
				&agent,
				None,
				&pinned_controller_key,
				&tokens,
				&session_key,
			)
			.await
		})
	}

	/// The controller side of the ephemeral handshake through the `HelloAck`: send a
	/// Challenge carrying a fresh ephemeral key, read the agent's Hello, and return
	/// the transcript both ends sign plus the Hello — so a mock controller signs the
	/// same message the agent does. `protocol`/`presented_key` are knobs for the
	/// rejection tests.
	async fn controller_challenge(
		c_send: &mut quinn::SendStream,
		c_recv: &mut quinn::RecvStream,
		protocol: u32,
		presented_key: String,
	) -> (Vec<u8>, Hello) {
		let eph = e2e::EphemeralKeypair::generate();
		let nonce = crypto::random_nonce_b64();
		write_frame(
			c_send,
			&Challenge {
				protocol,
				nonce: nonce.clone(),
				controller_key: presented_key,
				controller_eph: eph.public_b64(),
			},
		)
		.await
		.unwrap();
		let hello: Hello = read_frame_capped(c_recv, MAX_CONTROL_FRAME).await.unwrap();
		let agent_eph = e2e::decode_eph(&hello.agent_eph).expect("agent sends a valid ephemeral key");
		let transcript = e2e::handshake_transcript(&eph.public_bytes(), &agent_eph, &nonce, &hello.agent_nonce);
		(transcript, hello)
	}

	#[tokio::test]
	async fn mutual_handshake_succeeds_and_issues_a_capability_token() {
		let (_sep, server, _cep, client) = loopback().await;
		let ctrl = Identity::generate();
		let agent = Identity::generate();
		let tokens: TokenSet = Arc::new(Mutex::new(HashSet::new()));
		let session_key: SessionSlot = Arc::new(Mutex::new(None));

		// Pass a cloned handle so dropping the agent task at the end of the handshake
		// doesn't close the connection out from under the controller's final read
		// (in production `serve` holds the connection open in a loop).
		let handle = spawn_agent(
			client.clone(),
			agent,
			ctrl.public_b64(),
			tokens.clone(),
			session_key.clone(),
		);

		// An honest controller: presents its real key, verifies the agent's signature
		// over the transcript, and signs the same transcript with its identity key.
		let (mut c_send, mut c_recv) = server.open_bi().await.unwrap();
		let (transcript, hello) =
			controller_challenge(&mut c_send, &mut c_recv, PROTOCOL_VERSION, ctrl.public_b64()).await;
		assert_eq!(hello.protocol, PROTOCOL_VERSION);
		assert!(crypto::verify_b64(&hello.public_key, &transcript, &hello.signature));
		write_frame(
			&mut c_send,
			&HelloAck {
				accepted: true,
				reason: None,
				client_id: Some("cid-1".into()),
				controller_sig: ctrl.sign_b64(&transcript),
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
		// The end-to-end session key was agreed and stored for later streams.
		assert!(
			session_key.lock().unwrap().is_some(),
			"handshake must establish a session key"
		);
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
		let (mut c_send, mut c_recv) = server.open_bi().await.unwrap();
		let (transcript, _hello) =
			controller_challenge(&mut c_send, &mut c_recv, PROTOCOL_VERSION, presented_key).await;
		write_frame(
			&mut c_send,
			&HelloAck {
				accepted,
				reason: (!accepted).then(|| "rejected".to_string()),
				client_id: None,
				controller_sig: signer.sign_b64(&transcript),
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
		let session_key: SessionSlot = Arc::new(Mutex::new(None));

		let handle = spawn_agent(
			client,
			Identity::generate(),
			pinned.public_b64(),
			tokens.clone(),
			session_key.clone(),
		);
		// Imposter presents its own key (and signs with it) — a different key than
		// the agent pinned at enrollment.
		drive_dishonest_controller(&server, imposter.public_b64(), &imposter, true).await;

		assert!(handle.await.unwrap().is_err());
		assert!(
			tokens.lock().unwrap().is_empty(),
			"no token issued to an unverified controller"
		);
		assert!(
			session_key.lock().unwrap().is_none(),
			"no session key agreed with an unverified controller"
		);
	}

	#[tokio::test]
	async fn rejects_a_controller_protocol_version_mismatch() {
		let (_sep, server, _cep, client) = loopback().await;
		let ctrl = Identity::generate();
		let tokens: TokenSet = Arc::new(Mutex::new(HashSet::new()));
		let session_key: SessionSlot = Arc::new(Mutex::new(None));

		let handle = spawn_agent(
			client,
			Identity::generate(),
			ctrl.public_b64(),
			tokens.clone(),
			session_key.clone(),
		);
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
				controller_eph: e2e::EphemeralKeypair::generate().public_b64(),
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
		let session_key: SessionSlot = Arc::new(Mutex::new(None));

		let handle = spawn_agent(
			client,
			Identity::generate(),
			pinned.public_b64(),
			tokens.clone(),
			session_key.clone(),
		);
		// A peer that omits/blanks the controller key (the downgrade the
		// `#[serde(default)]` removal guards against) must be rejected.
		drive_dishonest_controller(&server, String::new(), &pinned, true).await;

		assert!(handle.await.unwrap().is_err());
		assert!(tokens.lock().unwrap().is_empty());
		assert!(session_key.lock().unwrap().is_none());
	}

	#[tokio::test]
	async fn rejects_a_bad_controller_signature() {
		let (_sep, server, _cep, client) = loopback().await;
		let ctrl = Identity::generate();
		let wrong = Identity::generate();
		let tokens: TokenSet = Arc::new(Mutex::new(HashSet::new()));
		let session_key: SessionSlot = Arc::new(Mutex::new(None));

		let handle = spawn_agent(
			client,
			Identity::generate(),
			ctrl.public_b64(),
			tokens.clone(),
			session_key.clone(),
		);
		// Correct key is presented, but the transcript is signed by the wrong key, so
		// the signature won't verify against the pinned key.
		drive_dishonest_controller(&server, ctrl.public_b64(), &wrong, true).await;

		assert!(handle.await.unwrap().is_err());
		assert!(tokens.lock().unwrap().is_empty());
		assert!(session_key.lock().unwrap().is_none());
	}

	#[tokio::test]
	async fn rejects_when_the_controller_declines_the_agent() {
		let (_sep, server, _cep, client) = loopback().await;
		let ctrl = Identity::generate();
		let tokens: TokenSet = Arc::new(Mutex::new(HashSet::new()));
		let session_key: SessionSlot = Arc::new(Mutex::new(None));

		let handle = spawn_agent(
			client,
			Identity::generate(),
			ctrl.public_b64(),
			tokens.clone(),
			session_key.clone(),
		);
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
		// A shared end-to-end key: the tunnel now runs over the encrypted channel, so
		// both ends wrap their halves before piping (the salt exchange derives the
		// per-stream directional keys).
		let key = SessionKey::derive(&[7u8; 32], b"tunnel-test");

		// Keep `client` alive in the test; the task gets a clone so finishing the
		// tunnel doesn't tear the connection down before the controller's read.
		let agent_conn = client.clone();
		let agent_key = key.clone();
		let agent = tokio::spawn(async move {
			let (send, recv) = agent_conn.accept_bi().await.unwrap();
			let (send, recv) = e2e::open_secure_agent(send, recv, &agent_key).await.unwrap();
			tunnel(port, send, recv).await;
		});

		let (send, recv) = server.open_bi().await.unwrap();
		let (mut send, mut recv) = e2e::open_secure_controller(send, recv, &key).await.unwrap();
		send.write_all(b"hello-tunnel").await.unwrap();
		let _ = send.shutdown().await;
		let mut echoed = Vec::new();
		recv.read_to_end(&mut echoed).await.unwrap();
		assert_eq!(echoed, b"hello-tunnel");
		agent.await.unwrap();
	}

	// The agent side of a peer-to-peer punch: `punch_to_controller` must dial the
	// controller directly and complete the full mutual handshake over that path (via
	// `serve_direct`), so the controller ends up with an authenticated direct link.
	#[tokio::test]
	async fn punch_to_controller_dials_and_completes_the_direct_handshake() {
		tls::install_crypto_provider();
		let controller_id = Identity::generate();
		let controller_key = Arc::new(controller_id.public_b64());
		let agent_id = Arc::new(Identity::generate());

		// A mock controller: a server endpoint that accepts the agent's direct dial and
		// runs the controller side of the handshake, then closes so `serve_direct` ends.
		let (cert, key) = tls::self_signed();
		let controller_ep = Endpoint::server(tls::server_config(cert, key), (Ipv4Addr::LOCALHOST, 0).into()).unwrap();
		let addr: SocketAddr = (Ipv4Addr::LOCALHOST, controller_ep.local_addr().unwrap().port()).into();

		let controller_task = tokio::spawn(async move {
			let conn = controller_ep.accept().await.unwrap().accept().unwrap().await.unwrap();
			let (mut send, mut recv) = conn.open_bi().await.unwrap();
			write_frame(&mut send, &StreamOpen::Handshake).await.unwrap();
			let nonce = crypto::random_nonce_b64();
			let eph = e2e::EphemeralKeypair::generate();
			write_frame(
				&mut send,
				&Challenge {
					protocol: PROTOCOL_VERSION,
					nonce: nonce.clone(),
					controller_key: controller_id.public_b64(),
					controller_eph: eph.public_b64(),
				},
			)
			.await
			.unwrap();
			let hello: Hello = read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await.unwrap();
			let agent_eph = e2e::decode_eph(&hello.agent_eph).unwrap();
			let transcript = e2e::handshake_transcript(&eph.public_bytes(), &agent_eph, &nonce, &hello.agent_nonce);
			let agent_verified = crypto::verify_b64(&hello.public_key, &transcript, &hello.signature);
			write_frame(
				&mut send,
				&HelloAck {
					accepted: true,
					reason: None,
					client_id: Some("cid".into()),
					controller_sig: controller_id.sign_b64(&transcript),
				},
			)
			.await
			.unwrap();
			let _ = send.finish();
			let grant: SessionGrant = read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await.unwrap();
			// Done — close so the agent's `serve_direct` loop returns.
			conn.close(0u32.into(), b"done");
			(agent_verified, grant.token)
		});

		let agent_ep = tls::client_endpoint(addr).unwrap();
		// The agent acts on a punch signal: dial the controller directly and serve it.
		punch_to_controller(
			&agent_ep,
			&addr.to_string(),
			"rendezvous-id",
			agent_id.clone(),
			controller_key,
			"libretether.local",
		)
		.await;

		let (agent_verified, token) = controller_task.await.unwrap();
		assert!(
			agent_verified,
			"the controller verified the agent's signature over the direct path"
		);
		assert!(
			!token.is_empty(),
			"the agent issued a capability token for the direct path"
		);
	}

	// A punch signal naming an unparseable address must be ignored, not crash the
	// signal handler.
	#[tokio::test]
	async fn punch_to_a_garbage_address_is_ignored() {
		tls::install_crypto_provider();
		let agent_ep = tls::client_endpoint((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
		// Returns without panicking; nothing to connect to.
		punch_to_controller(
			&agent_ep,
			"not-an-address",
			"rv",
			Arc::new(Identity::generate()),
			Arc::new(Identity::generate().public_b64()),
			"libretether.local",
		)
		.await;
	}

	use tokio::io::{AsyncReadExt, AsyncWriteExt};
}
