//! LibreTether relay (`libretether-relay`).
//!
//! Run this on a public cloud host. The controller and the agents all dial out
//! to it; it authenticates each side (owner secret vs agent secret), tracks
//! agents by Ed25519 public key, and pipes streams between the controller and
//! the addressed agent. It never inspects stream contents — the LibreTether handshake,
//! control RPCs, live session and TCP tunnels are all end-to-end.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use clap::{Parser, Subcommand};
use libretether_common::{pipe_bidirectional, shutdown_signal};
use libretether_protocol::crypto::{self, random_alnum};
use libretether_protocol::frame::{read_frame_capped, write_frame, MAX_CONTROL_FRAME};
use libretether_protocol::relay::{
	RelayAck, RelayChallenge, RelayEvent, RelayHello, RelayProof, RelayRequest, RelayRole,
};
use libretether_protocol::{secret, tls, DEFAULT_PORT};
use quinn::{Endpoint, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

mod logbook;

/// Hard ceiling on concurrent connections we'll service at once — beyond this we
/// shed load rather than spawn unbounded tasks for a UDP-reachable public port.
const MAX_CONNECTIONS: usize = 1024;
/// Slots within [`MAX_CONNECTIONS`] kept out of reach of agents, so the controller
/// can always connect even while an agent-secret holder opens connections in bulk.
/// Agents acquire from a pool of `MAX_CONNECTIONS - CONTROLLER_RESERVED`; the
/// global semaphore (held from accept, before the role is known) still bounds the
/// total, so authenticated agents can occupy at most that many long-lived slots,
/// leaving this much headroom for the controller and in-flight handshakes.
const CONTROLLER_RESERVED: usize = 16;
/// How long a peer has to complete the QUIC handshake *and* the auth handshake
/// before we drop it, so a peer that connects and then stalls at either stage
/// can't tie up a connection slot.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the relay emits an application-level heartbeat to the connected
/// controller, so a wedged relay (QUIC still answering keep-alives, routing loop
/// stuck) is detected by the controller's read timeout. Must be well under the
/// controller's read timeout.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Per-source rate limit: at most this many new connections per IP per window.
const RATE_LIMIT_PER_WINDOW: u32 = 60;
const RATE_WINDOW: Duration = Duration::from_secs(10);
/// QUIC application error code the relay resets a routed stream with when the
/// addressed agent isn't connected, so the controller can tell "agent offline"
/// apart from a transport drop.
const AGENT_UNAVAILABLE: u32 = 0x10;

#[derive(Serialize, Deserialize)]
struct ServerConfig {
	listen_addr: String,
	owner_secret: String,
	agent_secret: String,
	cert_der: String,
	key_der: String,
}

impl ServerConfig {
	fn generate() -> Self {
		let (cert_der, key_der) = tls::self_signed();
		Self {
			listen_addr: format!("0.0.0.0:{DEFAULT_PORT}"),
			owner_secret: random_alnum(24),
			agent_secret: random_alnum(24),
			cert_der: B64.encode(cert_der),
			key_der: B64.encode(key_der),
		}
	}

	fn cert_key(&self) -> Result<(Vec<u8>, Vec<u8>)> {
		Ok((B64.decode(&self.cert_der)?, B64.decode(&self.key_der)?))
	}

	/// Refuse to operate with a blank secret. An empty `owner_secret`/`agent_secret`
	/// would make `ct_eq("", "")` true, i.e. authenticate any peer presenting an
	/// empty secret — a fail-open we reject outright (a freshly generated config is
	/// always valid; this only catches a hand-edited/truncated one).
	fn validate(&self) -> Result<()> {
		if self.owner_secret.trim().is_empty() || self.agent_secret.trim().is_empty() {
			anyhow::bail!("has a blank owner/agent secret — delete it to regenerate, or restore the secrets");
		}
		Ok(())
	}
}

fn config_path(arg: Option<PathBuf>) -> PathBuf {
	arg.unwrap_or_else(|| {
		dirs::config_dir()
			.unwrap_or_else(|| PathBuf::from("."))
			.join("libretether-relay")
			.join("config.json")
	})
}

/// Parse and validate a config from its on-disk text.
fn parse_config(raw: &str, path: &Path) -> Result<ServerConfig> {
	let cfg: ServerConfig = serde_json::from_str(raw).context("parsing server config")?;
	cfg.validate()
		.with_context(|| format!("config at {}", path.display()))?;
	Ok(cfg)
}

/// Read an existing config, never creating one. `info` uses this: generating a
/// fresh config here would print freshly-minted secrets that don't match the
/// running relay — e.g. when `--config` is omitted and a default path the relay
/// never used is consulted — which is more dangerous than a clean failure. Fail
/// closed with an actionable error instead.
fn load(path: &PathBuf) -> Result<ServerConfig> {
	match std::fs::read_to_string(path) {
		Ok(raw) => parse_config(&raw, path),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(
			"no relay config at {} — run the relay (`libretether-relay run`) first to \
			 generate one, or pass --config <path> to point at an existing config",
			path.display()
		),
		Err(e) => Err(anyhow::Error::new(e).context(format!("reading relay config at {}", path.display()))),
	}
}

/// Read the config, generating and persisting a fresh one on first run. `run`
/// uses this; `info` must not (see [`load`]).
fn load_or_create(path: &PathBuf) -> Result<ServerConfig> {
	match std::fs::read_to_string(path) {
		Ok(raw) => parse_config(&raw, path),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			let cfg = ServerConfig::generate();
			// The config holds the owner/agent secrets and TLS key — write it
			// owner-only so other local users on the relay host can't read them.
			secret::write_str(path, &serde_json::to_string_pretty(&cfg)?)?;
			Ok(cfg)
		}
		Err(e) => Err(e.into()),
	}
}

// ---------------------------------------------------------------- relay state

/// Hands out a unique generation to each controller session so a stale session's
/// teardown can't clear a newer session's event sender (see `serve_controller`).
static CONTROLLER_GEN: AtomicU64 = AtomicU64::new(0);

/// The single connected controller's session, tagged with its generation. We
/// keep its connection so a newer controller can tear the old one down.
struct ControllerSession {
	generation: u64,
	events: UnboundedSender<RelayEvent>,
	conn: quinn::Connection,
}

type ControllerSlot = Arc<Mutex<Option<ControllerSession>>>;

#[derive(Clone, Default)]
struct Relay {
	agents: Arc<Mutex<HashMap<String, quinn::Connection>>>,
	controller: ControllerSlot,
	/// Per-source-IP fixed-window connection counters for rate limiting.
	limiter: Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>,
}

impl Relay {
	fn agent(&self, public_key: &str) -> Option<quinn::Connection> {
		self.agents.lock().unwrap().get(public_key).cloned()
	}

	fn notify(&self, event: RelayEvent) {
		if let Some(session) = self.controller.lock().unwrap().as_ref() {
			let _ = session.events.send(event);
		}
	}

	/// Fixed-window per-IP rate check: returns false once a source exceeds
	/// [`RATE_LIMIT_PER_WINDOW`] new connections within [`RATE_WINDOW`].
	fn allow(&self, ip: IpAddr) -> bool {
		self.allow_at(ip, Instant::now())
	}

	/// [`Relay::allow`] with an injectable clock so the window rollover and stale
	/// eviction are deterministically testable.
	fn allow_at(&self, ip: IpAddr, now: Instant) -> bool {
		let mut map = self.limiter.lock().unwrap();
		// Opportunistically drop stale entries so the map can't grow unbounded.
		if map.len() > 10_000 {
			map.retain(|_, (_, t)| now.duration_since(*t) < RATE_WINDOW);
		}
		let entry = map.entry(ip).or_insert((0, now));
		if now.duration_since(entry.1) >= RATE_WINDOW {
			*entry = (0, now);
		}
		entry.0 += 1;
		entry.0 <= RATE_LIMIT_PER_WINDOW
	}
}

#[derive(Parser)]
#[command(name = "libretether-relay", version, about = "LibreTether relay server")]
struct Cli {
	/// Path to the server config file.
	#[arg(long, global = true)]
	config: Option<PathBuf>,

	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand)]
enum Command {
	/// Run the relay.
	Run {
		/// Override the listen address (e.g. 0.0.0.0:47600).
		#[arg(long)]
		listen: Option<String>,
	},
	/// Print the listen address and the owner/agent secrets.
	Info,
}

#[tokio::main]
async fn main() -> Result<()> {
	let cli = Cli::parse();
	let path = config_path(cli.config.clone());

	match cli.command {
		Command::Info => {
			// Read-only: never generate a config, so we can't print secrets that
			// wouldn't match the running relay — see `load`.
			print_credentials(&load(&path)?);
			Ok(())
		}
		Command::Run { listen } => {
			let mut cfg = load_or_create(&path)?;
			if let Some(listen) = listen {
				cfg.listen_addr = listen;
			}
			run(cfg).await
		}
	}
}

fn print_credentials(cfg: &ServerConfig) {
	println!("listen:       {}", cfg.listen_addr);
	println!("owner secret: {}", cfg.owner_secret);
	println!("agent secret: {}", cfg.agent_secret);
	println!();
	println!("Point the controller at this host with the owner secret, and");
	println!("deploy clients with the agent secret.");
}

async fn run(cfg: ServerConfig) -> Result<()> {
	let (cert, key) = cfg.cert_key()?;
	let addr: SocketAddr = cfg.listen_addr.parse().context("invalid listen address")?;
	let endpoint = Endpoint::server(tls::server_config(cert, key), addr)?;
	logbook::info(&format!("relay listening on udp/{addr}"));
	// Don't echo the secrets on every `run` — they'd persist in the journal /
	// `docker logs` for the life of the deployment. `libretether-relay info` prints
	// them on demand.
	logbook::info("run `libretether-relay info` to print the owner/agent secrets");

	let relay = Relay::default();
	let secrets = Arc::new((cfg.owner_secret, cfg.agent_secret));
	let conn_limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));
	// Agents draw from a smaller pool so they can never consume the controller's
	// reserved headroom (see CONTROLLER_RESERVED). The role isn't known until after
	// auth, so this is acquired inside `handle` once the peer proves it's an agent.
	let agent_limit = Arc::new(Semaphore::new(MAX_CONNECTIONS - CONTROLLER_RESERVED));

	loop {
		tokio::select! {
			incoming = endpoint.accept() => {
				let Some(incoming) = incoming else { break };
				// Shed obvious abuse before doing any handshake work: rate-limit per
				// source IP, then cap total concurrent connections.
				if !relay.allow(incoming.remote_address().ip()) {
					incoming.refuse();
					continue;
				}
				let Ok(permit) = conn_limit.clone().try_acquire_owned() else {
					incoming.refuse();
					continue;
				};
				let relay = relay.clone();
				let secrets = secrets.clone();
				let agent_limit = agent_limit.clone();
				tokio::spawn(async move {
					let _permit = permit; // released when the connection task ends
					if let Err(e) = handle(relay, incoming, &secrets, &agent_limit).await {
						logbook::warn(&format!("connection error: {e}"));
					}
				});
			}
			_ = shutdown_signal() => {
				logbook::info("shutting down");
				break;
			}
		}
	}
	// Tell peers we're going away so they reconnect promptly instead of waiting
	// out the idle timeout, then exit cleanly (so `docker stop` is graceful).
	endpoint.close(0u32.into(), b"relay shutting down");
	Ok(())
}

async fn handle(
	relay: Relay,
	incoming: quinn::Incoming,
	secrets: &(String, String),
	agent_limit: &Arc<Semaphore>,
) -> Result<()> {
	// Bound the whole pre-serve phase — the QUIC/TLS handshake AND the app-level
	// auth — under one timeout. The connection permit is acquired at accept (before
	// either runs), so a peer that completes the UDP path then stalls at *either*
	// stage must not be able to hold that permit for longer than HANDSHAKE_TIMEOUT.
	let pre = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
		let conn = incoming.accept()?.await?;
		let authed = authenticate(&conn, secrets).await?;
		Ok::<_, anyhow::Error>((conn, authed))
	})
	.await;
	let (conn, authed) = match pre {
		Ok(Ok((conn, Some(authed)))) => (conn, authed),
		Ok(Ok((_, None))) => return Ok(()), // cleanly rejected (bad secret / proof)
		Ok(Err(e)) => return Err(e),        // QUIC or I/O error during the handshake
		Err(_) => return Ok(()),            // QUIC handshake or auth timed out
	};

	match authed.role {
		RelayRole::Controller => serve_controller(relay, conn, authed.send).await,
		RelayRole::Agent => {
			// Reserve controller headroom: agents acquire from the smaller agent pool
			// so an agent-secret holder opening connections in bulk (even under freshly
			// minted keys, which the key-ownership proof can't prevent) can't drain the
			// global pool and lock the controller out. Held for the connection's life.
			let Ok(permit) = agent_limit.clone().try_acquire_owned() else {
				logbook::warn("agent connection refused: at agent capacity");
				return Ok(());
			};
			serve_agent(relay, conn, authed.public_key, permit).await
		}
	}
}

/// A successfully-authenticated relay peer.
struct Authed {
	role: RelayRole,
	/// The hello stream's send half — the controller keeps writing presence events on it.
	send: SendStream,
	public_key: String,
}

/// Validate a peer's secret and prove it holds the private key for the public key
/// it presented. Returns `Some` on success, `None` if cleanly rejected.
async fn authenticate(conn: &quinn::Connection, secrets: &(String, String)) -> Result<Option<Authed>> {
	let (mut send, mut recv) = conn.accept_bi().await.context("accept hello stream")?;
	let hello: RelayHello = read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await?;

	let (expected, role_label) = match hello.role {
		RelayRole::Controller => (&secrets.0, "controller"),
		RelayRole::Agent => (&secrets.1, "agent"),
	};
	// Constant-time compare so the secret can't be recovered byte-by-byte via
	// response timing.
	if !crypto::ct_eq(&hello.secret, expected) {
		let _ = write_frame(
			&mut send,
			&RelayAck {
				accepted: false,
				reason: Some("bad secret".into()),
			},
		)
		.await;
		return Ok(None);
	}

	// Prove possession of the presented Ed25519 key before trusting it — in
	// particular before registering an agent under it for routing, so a holder of
	// the (shared) agent secret can't hijack another agent's key.
	let nonce = crypto::random_nonce_b64();
	write_frame(&mut send, &RelayChallenge { nonce: nonce.clone() }).await?;
	let proof: RelayProof = read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await?;
	if !crypto::verify_b64(&hello.public_key, nonce.as_bytes(), &proof.signature) {
		let _ = write_frame(
			&mut send,
			&RelayAck {
				accepted: false,
				reason: Some("key ownership proof failed".into()),
			},
		)
		.await;
		return Ok(None);
	}

	write_frame(
		&mut send,
		&RelayAck {
			accepted: true,
			reason: None,
		},
	)
	.await?;
	logbook::info(&format!(
		"{role_label} connected ({}…)",
		&hello.public_key.chars().take(8).collect::<String>()
	));

	// Route on the canonical key bytes, not the raw wire string: two base64
	// encodings of the same key must resolve to one routing entry. `verify_b64`
	// already proved it's a real 32-byte key, so canonicalization can't fail here.
	let public_key = crypto::canonical_pubkey(&hello.public_key).unwrap_or(hello.public_key);

	Ok(Some(Authed {
		role: hello.role,
		send,
		public_key,
	}))
}

/// The controller pushes presence events out on `events`, and opens one routed
/// bi stream per request which we pipe to the addressed agent.
async fn serve_controller(relay: Relay, conn: quinn::Connection, mut events: SendStream) -> Result<()> {
	let generation = CONTROLLER_GEN.fetch_add(1, Ordering::Relaxed);
	let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RelayEvent>();

	// Install our sender (and connection) and snapshot existing agents under one
	// lock hold: doing it atomically means an agent that connects mid-attach is
	// delivered via the sender rather than missed (presence race). Any previous
	// controller is displaced and its connection closed, so a second owner can't
	// leave a zombie routing loop running.
	let previous = {
		let agents = relay.agents.lock().unwrap();
		let mut slot = relay.controller.lock().unwrap();
		let previous = slot.replace(ControllerSession {
			generation,
			events: tx.clone(),
			conn: conn.clone(),
		});
		for key in agents.keys() {
			let _ = tx.send(RelayEvent::AgentOnline {
				public_key: key.clone(),
			});
		}
		previous
	};
	if let Some(prev) = previous {
		prev.conn.close(0u32.into(), b"replaced by a newer controller");
	}

	// Forward presence events to the controller.
	let events_task = tokio::spawn(async move {
		while let Some(event) = rx.recv().await {
			if write_frame(&mut events, &event).await.is_err() {
				break;
			}
		}
	});

	// Emit a periodic heartbeat so the controller can tell a healthy-but-idle relay
	// (no agents changing) from a wedged one. Stops when the event channel closes
	// (controller gone / `events_task` torn down).
	let heartbeat = {
		let tx = tx.clone();
		tokio::spawn(async move {
			let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
			ticker.tick().await; // the first tick fires immediately — skip it
			loop {
				ticker.tick().await;
				if tx.send(RelayEvent::Heartbeat).is_err() {
					break;
				}
			}
		})
	};

	// Each stream the controller opens leads with a RelayRequest header: either route
	// it to the named agent (the common case) or serve it ourselves (the relay's own
	// logs).
	loop {
		let (mut c_send, mut c_recv) = match conn.accept_bi().await {
			Ok(pair) => pair,
			Err(_) => break,
		};
		let relay = relay.clone();
		tokio::spawn(async move {
			let Ok(req) = read_frame_capped::<_, RelayRequest>(&mut c_recv, MAX_CONTROL_FRAME).await else {
				return;
			};
			match req {
				// Served by the relay itself: hand back a snapshot of its own log ring
				// so an operator can read the relay's activity from the controller.
				RelayRequest::FetchLogs { max_lines } => {
					let snapshot = logbook::snapshot(max_lines.map(|m| m as usize));
					let _ = write_frame(&mut c_send, &snapshot).await;
					let _ = c_send.finish();
				}
				// Pipe to the addressed agent. Reset the stream with a distinct code
				// (rather than silently dropping it) when the agent is gone or its
				// connection is dying, so the controller gets a prompt, attributable
				// failure instead of an opaque close it might mistake for a transient
				// relay hiccup.
				RelayRequest::Route { agent } => {
					let agent_key = crypto::canonical_pubkey(&agent).unwrap_or(agent);
					let Some(agent) = relay.agent(&agent_key) else {
						let _ = c_send.reset(AGENT_UNAVAILABLE.into());
						return;
					};
					match agent.open_bi().await {
						Ok((a_send, a_recv)) => pipe(c_recv, a_send, a_recv, c_send).await,
						Err(_) => {
							let _ = c_send.reset(AGENT_UNAVAILABLE.into());
						}
					}
				}
			}
		});
	}

	// Only relinquish the slot if it's still ours: a reconnecting controller may
	// have already installed a newer sender, and clearing that would kill its
	// event stream and bounce it into an endless reconnect loop.
	{
		let mut slot = relay.controller.lock().unwrap();
		if slot.as_ref().map(|s| s.generation) == Some(generation) {
			slot.take();
		}
	}
	events_task.abort();
	heartbeat.abort();
	Ok(())
}

/// `_permit` ties an agent-pool slot to this connection's lifetime (see
/// CONTROLLER_RESERVED): it is released when the connection ends.
async fn serve_agent(
	relay: Relay,
	conn: quinn::Connection,
	public_key: String,
	_permit: OwnedSemaphorePermit,
) -> Result<()> {
	let conn_id = conn.stable_id();
	// Register, displacing any prior connection under this key. An honest agent
	// holds exactly one connection; force-closing a stale predecessor frees its
	// slot/permit immediately instead of waiting for its own `closed()` to fire,
	// and bounds a single identity to one live connection.
	let previous = relay.agents.lock().unwrap().insert(public_key.clone(), conn.clone());
	if let Some(prev) = previous {
		if prev.stable_id() != conn_id {
			prev.close(0u32.into(), b"replaced by a newer connection for this key");
		}
	}
	relay.notify(RelayEvent::AgentOnline {
		public_key: public_key.clone(),
	});

	conn.closed().await;

	// Only deregister if we're still the registered connection for this key. A
	// reconnect can replace us with a fresh connection before our `closed()`
	// fires; removing then would wrongly mark a live agent offline (stale-cleanup
	// race), and the agent would stay unreachable until its new connection drops.
	let mut agents = relay.agents.lock().unwrap();
	if agents.get(&public_key).map(|c| c.stable_id()) == Some(conn_id) {
		agents.remove(&public_key);
		drop(agents);
		relay.notify(RelayEvent::AgentOffline { public_key });
	}
	Ok(())
}

/// Pipe a controller stream and an agent stream together until both close.
/// Tearing both halves down as soon as one finishes would truncate a
/// request/response (the controller finishes its send half right after the
/// request) or end a live session early, so the shared helper waits for both —
/// see [`libretether_common::pipe_bidirectional`].
async fn pipe(c_recv: RecvStream, a_send: SendStream, a_recv: RecvStream, c_send: SendStream) {
	pipe_bidirectional(c_recv, c_send, a_recv, a_send).await;
}

#[cfg(test)]
mod tests {
	use super::*;
	use libretether_protocol::crypto::Identity;
	use std::net::Ipv4Addr;

	/// A standalone agent-pool permit for tests that call `serve_agent` directly
	/// (in production the permit comes from the shared agent semaphore in `handle`).
	fn agent_permit() -> OwnedSemaphorePermit {
		Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap()
	}

	#[test]
	fn rate_limiter_allows_a_burst_then_sheds_per_source() {
		let relay = Relay::default();
		let ip: IpAddr = "203.0.113.7".parse().unwrap();
		// Up to the window limit is allowed.
		for _ in 0..RATE_LIMIT_PER_WINDOW {
			assert!(relay.allow(ip));
		}
		// The next connection in the same window is shed.
		assert!(!relay.allow(ip));
		// A different source has its own independent budget.
		let other: IpAddr = "203.0.113.8".parse().unwrap();
		assert!(relay.allow(other));
	}

	/// A connected QUIC pair: the relay is the server (it accepts), the peer is the
	/// client (it opens the hello stream). Endpoints are returned so callers keep
	/// them (and the connections) alive for the test's duration.
	async fn loopback() -> (Endpoint, quinn::Connection, Endpoint, quinn::Connection) {
		tls::install_crypto_provider();
		let (cert, key) = tls::self_signed();
		let relay_ep = Endpoint::server(tls::server_config(cert, key), (Ipv4Addr::LOCALHOST, 0).into()).unwrap();
		let addr = relay_ep.local_addr().unwrap();
		let peer_ep = tls::client_endpoint(addr).unwrap();
		let accept = {
			let ep = relay_ep.clone();
			tokio::spawn(async move { ep.accept().await.unwrap().accept().unwrap().await.unwrap() })
		};
		let peer_conn = peer_ep.connect(addr, "libretether.local").unwrap().await.unwrap();
		let relay_conn = accept.await.unwrap();
		(relay_ep, relay_conn, peer_ep, peer_conn)
	}

	#[tokio::test]
	async fn authenticate_accepts_a_controller_with_owner_secret_and_valid_proof() {
		let (_rep, relay_conn, _pep, peer_conn) = loopback().await;
		let secrets: (String, String) = ("owner-secret".into(), "agent-secret".into());
		let id = Identity::generate();
		let id_pub = id.public_b64();

		// Honest controller: owner secret + a signature proving it holds the key.
		let peer = tokio::spawn(async move {
			let (mut s, mut r) = peer_conn.open_bi().await.unwrap();
			write_frame(
				&mut s,
				&RelayHello {
					role: RelayRole::Controller,
					secret: "owner-secret".into(),
					public_key: id.public_b64(),
				},
			)
			.await
			.unwrap();
			let ch: RelayChallenge = read_frame_capped(&mut r, MAX_CONTROL_FRAME).await.unwrap();
			write_frame(
				&mut s,
				&RelayProof {
					signature: id.sign_b64(ch.nonce.as_bytes()),
				},
			)
			.await
			.unwrap();
			let ack: RelayAck = read_frame_capped(&mut r, MAX_CONTROL_FRAME).await.unwrap();
			(ack, peer_conn)
		});

		let authed = authenticate(&relay_conn, &secrets)
			.await
			.unwrap()
			.expect("should authenticate");
		assert!(matches!(authed.role, RelayRole::Controller));
		assert_eq!(authed.public_key, id_pub);

		let (ack, _peer) = peer.await.unwrap();
		assert!(ack.accepted);
	}

	#[tokio::test]
	async fn authenticate_rejects_a_bad_secret() {
		let (_rep, relay_conn, _pep, peer_conn) = loopback().await;
		let secrets: (String, String) = ("owner-secret".into(), "agent-secret".into());
		let id = Identity::generate();

		let peer = tokio::spawn(async move {
			let (mut s, mut r) = peer_conn.open_bi().await.unwrap();
			write_frame(
				&mut s,
				&RelayHello {
					role: RelayRole::Agent,
					secret: "wrong-secret".into(),
					public_key: id.public_b64(),
				},
			)
			.await
			.unwrap();
			// Bad secret short-circuits to an ack (no challenge is sent).
			let ack: RelayAck = read_frame_capped(&mut r, MAX_CONTROL_FRAME).await.unwrap();
			(ack, peer_conn)
		});

		assert!(authenticate(&relay_conn, &secrets).await.unwrap().is_none());
		let (ack, _peer) = peer.await.unwrap();
		assert!(!ack.accepted);
	}

	#[tokio::test]
	async fn authenticate_rejects_a_bad_key_ownership_proof() {
		let (_rep, relay_conn, _pep, peer_conn) = loopback().await;
		let secrets: (String, String) = ("owner-secret".into(), "agent-secret".into());
		let id = Identity::generate();
		let imposter = Identity::generate();

		// Correct agent secret but the proof is signed by a different key — so the
		// peer can't register under `id`'s routing key (the hijack the proof blocks).
		let peer = tokio::spawn(async move {
			let (mut s, mut r) = peer_conn.open_bi().await.unwrap();
			write_frame(
				&mut s,
				&RelayHello {
					role: RelayRole::Agent,
					secret: "agent-secret".into(),
					public_key: id.public_b64(),
				},
			)
			.await
			.unwrap();
			let ch: RelayChallenge = read_frame_capped(&mut r, MAX_CONTROL_FRAME).await.unwrap();
			write_frame(
				&mut s,
				&RelayProof {
					signature: imposter.sign_b64(ch.nonce.as_bytes()),
				},
			)
			.await
			.unwrap();
			let ack: RelayAck = read_frame_capped(&mut r, MAX_CONTROL_FRAME).await.unwrap();
			(ack, peer_conn)
		});

		assert!(authenticate(&relay_conn, &secrets).await.unwrap().is_none());
		let (ack, _peer) = peer.await.unwrap();
		assert!(!ack.accepted);
	}

	// ------------------------------------------------------------ rate limiter

	#[test]
	fn rate_limiter_resets_after_the_window_elapses() {
		let relay = Relay::default();
		let ip: IpAddr = "203.0.113.9".parse().unwrap();
		let t0 = Instant::now();
		// Exhaust the window's budget at a fixed instant.
		for _ in 0..RATE_LIMIT_PER_WINDOW {
			assert!(relay.allow_at(ip, t0));
		}
		assert!(!relay.allow_at(ip, t0), "shed once the window budget is spent");
		// A check past the window rolls over to a fresh budget.
		let t1 = t0 + RATE_WINDOW + Duration::from_millis(1);
		assert!(relay.allow_at(ip, t1), "a new window grants a fresh budget");
	}

	#[test]
	fn rate_limiter_evicts_stale_entries_when_the_map_grows() {
		let relay = Relay::default();
		let t0 = Instant::now();
		// Seed the limiter past its eviction threshold with stale entries.
		{
			let mut map = relay.limiter.lock().unwrap();
			for i in 0..10_001u32 {
				map.insert(IpAddr::from(std::net::Ipv4Addr::from(i)), (1, t0));
			}
		}
		// A check well past the window triggers the opportunistic retain.
		let later = t0 + RATE_WINDOW + Duration::from_secs(1);
		assert!(relay.allow_at("198.51.100.1".parse().unwrap(), later));
		let len = relay.limiter.lock().unwrap().len();
		assert!(len < 10_001, "stale entries should be evicted, map still has {len}");
	}

	// ------------------------------------------------------ routing harness

	/// A relay server endpoint bound to loopback, plus its address.
	fn relay_server() -> (Endpoint, SocketAddr) {
		tls::install_crypto_provider();
		let (cert, key) = tls::self_signed();
		let ep = Endpoint::server(tls::server_config(cert, key), (Ipv4Addr::LOCALHOST, 0).into()).unwrap();
		let addr = ep.local_addr().unwrap();
		(ep, addr)
	}

	/// Dial `relay_ep` from a fresh client; returns the client endpoint (the caller
	/// keeps it alive), the client's connection, and the relay's view of it.
	async fn connect(relay_ep: &Endpoint, addr: SocketAddr) -> (Endpoint, quinn::Connection, quinn::Connection) {
		let client_ep = tls::client_endpoint(addr).unwrap();
		let accept = {
			let ep = relay_ep.clone();
			tokio::spawn(async move { ep.accept().await.unwrap().accept().unwrap().await.unwrap() })
		};
		let client_conn = client_ep.connect(addr, "libretether.local").unwrap().await.unwrap();
		let relay_view = accept.await.unwrap();
		(client_ep, client_conn, relay_view)
	}

	/// Open the controller's "hello" stream and hand back the relay-side send half
	/// `serve_controller` writes presence events on, plus the client-side recv half
	/// the controller reads them from. (Auth is exercised separately; this skips it
	/// to test routing in isolation.)
	async fn open_events(
		ctrl_conn: &quinn::Connection,
		ctrl_view: &quinn::Connection,
	) -> (quinn::SendStream, quinn::RecvStream) {
		let (mut hello_send, hello_recv) = ctrl_conn.open_bi().await.unwrap();
		hello_send.write_all(b"\x00").await.unwrap(); // materialize the stream so the relay accepts it
		let (events_send, _events_recv) = ctrl_view.accept_bi().await.unwrap();
		(events_send, hello_recv)
	}

	/// Poll `cond` until true, failing the test if it never becomes true.
	async fn wait_until(mut cond: impl FnMut() -> bool) {
		for _ in 0..400 {
			if cond() {
				return;
			}
			tokio::time::sleep(Duration::from_millis(5)).await;
		}
		panic!("condition was not met within the timeout");
	}

	async fn with_timeout<T>(label: &str, fut: impl std::future::Future<Output = T>) -> T {
		tokio::time::timeout(Duration::from_secs(5), fut)
			.await
			.unwrap_or_else(|_| panic!("{label} timed out"))
	}

	#[tokio::test]
	async fn routes_a_controller_stream_to_the_addressed_agent_both_ways() {
		let (relay_ep, addr) = relay_server();
		let relay = Relay::default();
		let agent_key = "AGENT_PUBKEY".to_string();

		// Register an agent.
		let (_aep, agent_conn, agent_view) = connect(&relay_ep, addr).await;
		tokio::spawn({
			let (relay, key) = (relay.clone(), agent_key.clone());
			async move { serve_agent(relay, agent_view, key, agent_permit()).await }
		});
		wait_until(|| relay.agent(&agent_key).is_some()).await;

		// Bring up a controller and start serving it.
		let (_cep, ctrl_conn, ctrl_view) = connect(&relay_ep, addr).await;
		let (events_send, _events_recv) = open_events(&ctrl_conn, &ctrl_view).await;
		tokio::spawn({
			let relay = relay.clone();
			async move { serve_controller(relay, ctrl_view, events_send).await }
		});

		// Controller opens a routed stream to the agent and sends a payload.
		let (mut rsend, mut rrecv) = ctrl_conn.open_bi().await.unwrap();
		write_frame(
			&mut rsend,
			&RelayRequest::Route {
				agent: agent_key.clone(),
			},
		)
		.await
		.unwrap();
		rsend.write_all(b"PING").await.unwrap();
		let _ = rsend.finish();

		// The agent receives the payload *without* the RelayRequest header (the relay
		// consumed it), then echoes back through the relay to the controller.
		let (mut asend, mut arecv) = with_timeout("agent accept", agent_conn.accept_bi()).await.unwrap();
		let got = with_timeout("agent read", arecv.read_to_end(64)).await.unwrap();
		assert_eq!(
			got, b"PING",
			"the RelayRequest header must be stripped; agent sees only the payload"
		);
		asend.write_all(b"PONG").await.unwrap();
		let _ = asend.finish();

		let back = with_timeout("controller read", rrecv.read_to_end(64)).await.unwrap();
		assert_eq!(back, b"PONG");
	}

	#[tokio::test]
	async fn a_routed_stream_for_an_unknown_agent_is_dropped() {
		let (relay_ep, addr) = relay_server();
		let relay = Relay::default();

		let (_cep, ctrl_conn, ctrl_view) = connect(&relay_ep, addr).await;
		let (events_send, _events_recv) = open_events(&ctrl_conn, &ctrl_view).await;
		tokio::spawn({
			let relay = relay.clone();
			async move { serve_controller(relay, ctrl_view, events_send).await }
		});

		// Route to an agent that was never registered: the relay resets the stream
		// with AGENT_UNAVAILABLE, so the controller gets an attributable failure
		// rather than an ambiguous clean close.
		let (mut rsend, mut rrecv) = ctrl_conn.open_bi().await.unwrap();
		write_frame(&mut rsend, &RelayRequest::Route { agent: "GHOST".into() })
			.await
			.unwrap();
		rsend.write_all(b"hello?").await.unwrap();
		let _ = rsend.finish();
		let result = with_timeout("read after route-to-unknown", rrecv.read_to_end(64)).await;
		match result {
			Err(quinn::ReadToEndError::Read(quinn::ReadError::Reset(code))) => {
				assert_eq!(
					code,
					AGENT_UNAVAILABLE.into(),
					"reset carries the agent-unavailable code"
				);
			}
			other => panic!("expected a stream reset for an unknown agent, got {other:?}"),
		}
	}

	#[tokio::test]
	async fn a_fetch_logs_request_is_served_by_the_relay_itself() {
		let (relay_ep, addr) = relay_server();
		let relay = Relay::default();

		// Seed the relay's own log ring so the snapshot has something to return.
		logbook::info("relay listening on udp/0.0.0.0:47600");

		let (_cep, ctrl_conn, ctrl_view) = connect(&relay_ep, addr).await;
		let (events_send, _events_recv) = open_events(&ctrl_conn, &ctrl_view).await;
		tokio::spawn({
			let relay = relay.clone();
			async move { serve_controller(relay, ctrl_view, events_send).await }
		});

		// A FetchLogs stream is answered by the relay itself (not routed to an agent):
		// it replies with a LogsResult drawn from its own log buffer.
		let (mut rsend, mut rrecv) = ctrl_conn.open_bi().await.unwrap();
		write_frame(&mut rsend, &RelayRequest::FetchLogs { max_lines: None })
			.await
			.unwrap();
		let _ = rsend.finish();
		let result: libretether_protocol::LogsResult =
			with_timeout("read relay logs", read_frame_capped(&mut rrecv, MAX_CONTROL_FRAME))
				.await
				.unwrap();
		assert!(
			result.lines.iter().any(|l| l.message.contains("relay listening")),
			"the relay returns its own recorded log lines"
		);
	}

	#[test]
	fn info_load_refuses_to_create_a_missing_config() {
		// `info` reads through `load`, which must fail with an actionable error
		// (rather than silently minting secrets) when no config exists, and must
		// leave no file behind — printing secrets the running relay never used
		// would be a footgun.
		let path = std::env::temp_dir()
			.join(format!("libretether-relay-load-test-{}", std::process::id()))
			.join("config.json");
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
		// Not `unwrap_err`: `ServerConfig` deliberately isn't `Debug` (it holds the
		// secrets), so match rather than require a `Debug` bound on the Ok type.
		let err = match load(&path) {
			Ok(_) => panic!("load must fail when no config exists"),
			Err(e) => e.to_string(),
		};
		assert!(
			err.contains("no relay config"),
			"expected an actionable error, got: {err}"
		);
		assert!(!path.exists(), "load must not create a config file");
	}

	#[test]
	fn config_validate_rejects_a_blank_secret() {
		let mut cfg = ServerConfig::generate();
		assert!(cfg.validate().is_ok(), "a freshly generated config is valid");
		cfg.owner_secret = "   ".into();
		assert!(cfg.validate().is_err(), "a blank owner secret must be rejected");
		let mut cfg = ServerConfig::generate();
		cfg.agent_secret = String::new();
		assert!(cfg.validate().is_err(), "a blank agent secret must be rejected");
	}

	#[tokio::test]
	async fn a_new_controller_displaces_and_closes_the_previous_one() {
		let (relay_ep, addr) = relay_server();
		let relay = Relay::default();

		// Controller A.
		let (_aep, ctrl_a, view_a) = connect(&relay_ep, addr).await;
		let (events_a, _ra) = open_events(&ctrl_a, &view_a).await;
		tokio::spawn({
			let relay = relay.clone();
			async move { serve_controller(relay, view_a, events_a).await }
		});
		wait_until(|| relay.controller.lock().unwrap().is_some()).await;
		let gen_a = relay.controller.lock().unwrap().as_ref().unwrap().generation;

		// Controller B connects and must displace A.
		let (_bep, ctrl_b, view_b) = connect(&relay_ep, addr).await;
		let (events_b, _rb) = open_events(&ctrl_b, &view_b).await;
		tokio::spawn({
			let relay = relay.clone();
			async move { serve_controller(relay, view_b, events_b).await }
		});
		wait_until(|| relay.controller.lock().unwrap().as_ref().map(|s| s.generation) != Some(gen_a)).await;

		// A's connection is force-closed by the relay (no zombie routing loop).
		with_timeout("controller A closed", ctrl_a.closed()).await;
	}

	#[tokio::test]
	async fn a_reconnecting_agent_keeps_the_newer_connection_registered() {
		let (relay_ep, addr) = relay_server();
		let relay = Relay::default();
		let key = "AGENT".to_string();

		// First connection C1 registers under the key.
		let (_e1, agent1, view1) = connect(&relay_ep, addr).await;
		let id1 = view1.stable_id();
		tokio::spawn({
			let (relay, key) = (relay.clone(), key.clone());
			async move { serve_agent(relay, view1, key, agent_permit()).await }
		});
		wait_until(|| relay.agent(&key).map(|c| c.stable_id()) == Some(id1)).await;

		// C2 (a reconnect) registers under the same key, replacing C1 in the map.
		let (_e2, agent2, view2) = connect(&relay_ep, addr).await;
		let id2 = view2.stable_id();
		assert_ne!(id1, id2);
		tokio::spawn({
			let (relay, key) = (relay.clone(), key.clone());
			async move { serve_agent(relay, view2, key, agent_permit()).await }
		});
		wait_until(|| relay.agent(&key).map(|c| c.stable_id()) == Some(id2)).await;

		// Now C1 drops. Its teardown must NOT deregister the key — the live
		// connection is C2 (the stable-id guard). C2 stays reachable.
		agent1.close(0u32.into(), b"bye");
		tokio::time::sleep(Duration::from_millis(100)).await;
		assert_eq!(
			relay.agent(&key).map(|c| c.stable_id()),
			Some(id2),
			"the reconnected agent (C2) must remain registered after the stale C1 drops"
		);
		let _ = agent2; // kept alive for the duration of the test
	}

	#[tokio::test]
	async fn controller_is_notified_when_an_agent_comes_online_and_goes_offline() {
		let (relay_ep, addr) = relay_server();
		let relay = Relay::default();

		// Controller attaches and starts reading presence events.
		let (_cep, ctrl_conn, ctrl_view) = connect(&relay_ep, addr).await;
		let (events_send, mut hello_recv) = open_events(&ctrl_conn, &ctrl_view).await;
		tokio::spawn({
			let relay = relay.clone();
			async move { serve_controller(relay, ctrl_view, events_send).await }
		});
		wait_until(|| relay.controller.lock().unwrap().is_some()).await;

		// Agent comes online → AgentOnline reaches the controller.
		let key = "AGENT".to_string();
		let (_aep, agent_conn, agent_view) = connect(&relay_ep, addr).await;
		tokio::spawn({
			let (relay, key) = (relay.clone(), key.clone());
			async move { serve_agent(relay, agent_view, key, agent_permit()).await }
		});
		let online: RelayEvent = with_timeout("online event", read_frame_capped(&mut hello_recv, MAX_CONTROL_FRAME))
			.await
			.unwrap();
		assert!(matches!(online, RelayEvent::AgentOnline { public_key } if public_key == key));

		// Agent drops → AgentOffline reaches the controller.
		agent_conn.close(0u32.into(), b"bye");
		let offline: RelayEvent = with_timeout("offline event", read_frame_capped(&mut hello_recv, MAX_CONTROL_FRAME))
			.await
			.unwrap();
		assert!(matches!(offline, RelayEvent::AgentOffline { public_key } if public_key == key));
	}
}
