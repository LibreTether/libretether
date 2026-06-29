//! The agent's networking core: dial the controller, complete the auth
//! handshake, then service control + session streams until the link drops, with
//! exponential reconnect backoff.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use libretether_protocol::crypto::{self, Identity};
use libretether_protocol::frame::{read_frame_capped, write_frame, MAX_CONTROL_FRAME};
use libretether_protocol::relay::{RelayAck, RelayChallenge, RelayHello, RelayProof, RelayRole};
use libretether_protocol::{
	tls, Challenge, ControlRequest, Hello, HelloAck, SessionGrant, StreamAuth, StreamOpen, PROTOCOL_VERSION,
};
use quinn::{Endpoint, RecvStream, SendStream};
use tokio::io::AsyncWriteExt;

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

static LOG_FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

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

pub fn log(msg: &str) {
	let line = format!("[libretether-agent] {msg}");
	eprintln!("{line}");
	if let Some(file) = LOG_FILE.get() {
		if let Ok(mut file) = file.lock() {
			let _ = writeln!(file, "{line}");
		}
	}
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
		_ = shutdown_signal() => log("shutdown signal received; exiting"),
	}
	Ok(())
}

/// Dial + serve forever, reconnecting with capped backoff.
async fn connect_loop(cfg: &mut AgentConfig, cfg_path: &PathBuf) {
	let mut backoff = 1u64;
	loop {
		match connect_once(cfg, cfg_path).await {
			Ok(()) => {
				log("controller connection closed");
				backoff = 1;
			}
			Err(e) => log(&format!("connection error: {e:#}")),
		}
		let wait = backoff.min(RECONNECT_MAX_SECS);
		log(&format!("reconnecting in {wait}s"));
		tokio::time::sleep(Duration::from_secs(wait)).await;
		backoff = (backoff * 2).min(RECONNECT_MAX_SECS);
	}
}

/// Resolve on the first SIGINT/SIGTERM (Ctrl+C on Windows) so the agent shuts
/// down cleanly instead of being force-killed.
async fn shutdown_signal() {
	#[cfg(unix)]
	{
		use tokio::signal::unix::{signal, SignalKind};
		let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
		let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
		tokio::select! {
			_ = term.recv() => {}
			_ = int.recv() => {}
		}
	}
	#[cfg(not(unix))]
	{
		let _ = tokio::signal::ctrl_c().await;
	}
}

fn make_endpoint(target: SocketAddr) -> Result<Endpoint> {
	// Bind the client socket to the target's address family: quinn rejects dialing
	// an IPv6 peer from an IPv4 socket (and vice versa) with "invalid remote
	// address", so an IPv6 relay/controller needs a [::] client, an IPv4 one 0.0.0.0.
	let bind: SocketAddr = if target.is_ipv6() {
		(std::net::Ipv6Addr::UNSPECIFIED, 0).into()
	} else {
		(std::net::Ipv4Addr::UNSPECIFIED, 0).into()
	};
	let mut endpoint = Endpoint::client(bind).context("binding client socket")?;
	endpoint.set_default_client_config(tls::client_config());
	Ok(endpoint)
}

async fn connect_once(cfg: &mut AgentConfig, cfg_path: &PathBuf) -> Result<()> {
	// Resolve the peer first so the client endpoint can match its address family.
	let (addr, is_relay) = match cfg.relay() {
		Some(relay) => (resolve(relay).await?, true),
		None => (resolve(&cfg.controller_addr).await?, false),
	};
	let endpoint = make_endpoint(addr)?;
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
	let (mut send, mut recv) = conn.open_bi().await.context("opening relay hello stream")?;
	let hello = RelayHello {
		role: RelayRole::Agent,
		secret: cfg.relay_secret.clone().unwrap_or_default(),
		public_key: identity.public_b64(),
	};
	write_frame(&mut send, &hello).await.context("sending relay hello")?;
	// Prove we hold the private key for the public key we presented, so a holder
	// of the (shared) agent secret can't register under our routing key.
	let challenge: RelayChallenge = read_frame_capped(&mut recv, MAX_CONTROL_FRAME)
		.await
		.context("reading relay challenge")?;
	let proof = RelayProof {
		signature: identity.sign_b64(challenge.nonce.as_bytes()),
	};
	write_frame(&mut send, &proof).await.context("sending relay proof")?;
	let ack: RelayAck = read_frame_capped(&mut recv, MAX_CONTROL_FRAME)
		.await
		.context("reading relay ack")?;
	if !ack.accepted {
		return Err(anyhow!("relay rejected agent: {}", ack.reason.unwrap_or_default()));
	}
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
async fn serve(conn: quinn::Connection, cfg: &mut AgentConfig, cfg_path: &PathBuf) -> Result<()> {
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
	let Some(expected) = cfg.controller_key.clone().filter(|k| !k.trim().is_empty()) else {
		return Err(anyhow!(
			"no controller key pinned in config — re-enroll with `--controller-key` (re-run the deploy command)"
		));
	};

	// Mutual handshake: prove our identity, verify the controller's against the
	// pinned key, and receive the capability token (issued into `tokens`) that
	// every later stream must carry.
	let (_controller_key, client_id) = match verify_and_grant(
		&mut send,
		&mut recv,
		&identity,
		cfg.enrollment_token.clone(),
		&expected,
		&tokens,
	)
	.await?
	{
		Some(v) => v,
		None => return Err(anyhow!("controller authentication failed")),
	};
	log(&format!(
		"authenticated as client {}",
		client_id.clone().unwrap_or_default()
	));

	// Burn the one-time enrollment token now that we're enrolled.
	if cfg.enrollment_token.is_some() {
		cfg.enrollment_token = None;
		cfg.client_id = client_id;
		if let Err(e) = cfg.save(cfg_path) {
			log(&format!("warning: could not persist enrolled config: {e}"));
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
/// per-connection capability token. Returns `(controller_key, client_id)`, or an
/// error if the controller is rejected or fails verification.
async fn verify_and_grant(
	send: &mut SendStream,
	recv: &mut RecvStream,
	identity: &Identity,
	enrollment_token: Option<String>,
	expected_key: &str,
	tokens: &Mutex<HashSet<String>>,
) -> Result<Option<(String, Option<String>)>> {
	let challenge: Challenge = read_frame_capped(recv, MAX_CONTROL_FRAME)
		.await
		.context("reading challenge")?;
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

	// Hand the verified controller a capability token for this connection.
	let token = crypto::random_nonce_b64();
	tokens.lock().unwrap().insert(token.clone());
	write_frame(send, &SessionGrant { token })
		.await
		.context("sending session grant")?;
	let _ = send.finish();
	Ok(Some((presented.to_string(), ack.client_id)))
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
		log("rejected stream: missing or invalid capability token");
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
		Ok(Some((_, client_id))) => log(&format!("re-authenticated as client {}", client_id.unwrap_or_default())),
		Ok(None) => {}
		Err(e) => log(&format!("controller re-auth rejected: {e:#}")),
	}
}

/// Pipe a QUIC stream to a local TCP port (the client's RDP/SSH server) — used
/// to reach the client through the relay.
async fn tunnel(port: u16, mut q_send: SendStream, mut q_recv: RecvStream) {
	match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
		Ok(tcp) => {
			let (mut tcp_read, mut tcp_write) = tcp.into_split();
			// Copy each direction independently and half-close it when its source
			// ends, then wait for BOTH — a shared select! tears the peer direction
			// down on the first EOF and truncates the stream.
			let up = async {
				let _ = tokio::io::copy(&mut q_recv, &mut tcp_write).await;
				let _ = tcp_write.shutdown().await;
			};
			let down = async {
				let _ = tokio::io::copy(&mut tcp_read, &mut q_send).await;
				let _ = q_send.finish();
			};
			tokio::join!(up, down);
		}
		Err(e) => {
			log(&format!("tunnel to 127.0.0.1:{port} failed: {e}"));
			let _ = q_send.finish();
		}
	}
}

async fn resolve(addr: &str) -> Result<SocketAddr> {
	if let Ok(sa) = addr.parse::<SocketAddr>() {
		return Ok(sa);
	}
	tokio::net::lookup_host(addr)
		.await
		.with_context(|| format!("resolving {addr}"))?
		.next()
		.ok_or_else(|| anyhow!("no address resolved for {addr}"))
}
