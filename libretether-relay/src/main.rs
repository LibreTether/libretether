//! LibreTether relay (`libretether-relay`).
//!
//! Run this on a public cloud host. The controller and the agents all dial out
//! to it; it authenticates each side (owner secret vs agent secret), tracks
//! agents by Ed25519 public key, and pipes streams between the controller and
//! the addressed agent. It never inspects stream contents — the LibreTether handshake,
//! control RPCs, live session and TCP tunnels are all end-to-end.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use clap::{Parser, Subcommand};
use libretether_common::shutdown_signal;
use libretether_protocol::crypto::{self, random_alnum};
use libretether_protocol::frame::{read_frame_capped, write_frame, MAX_CONTROL_FRAME};
use libretether_protocol::relay::{RelayAck, RelayChallenge, RelayEvent, RelayHello, RelayProof, RelayRole, RouteTo};
use libretether_protocol::{secret, tls, DEFAULT_PORT};
use quinn::{Endpoint, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Semaphore;

/// Hard ceiling on concurrent connections we'll service at once — beyond this we
/// shed load rather than spawn unbounded tasks for a UDP-reachable public port.
const MAX_CONNECTIONS: usize = 1024;
/// How long a peer has to complete the auth handshake before we drop it, so a
/// peer that connects and then stalls can't tie up a connection slot.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-source rate limit: at most this many new connections per IP per window.
const RATE_LIMIT_PER_WINDOW: u32 = 60;
const RATE_WINDOW: Duration = Duration::from_secs(10);

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
}

fn config_path(arg: Option<PathBuf>) -> PathBuf {
	arg.unwrap_or_else(|| {
		dirs::config_dir()
			.unwrap_or_else(|| PathBuf::from("."))
			.join("libretether-relay")
			.join("config.json")
	})
}

fn load_or_create(path: &PathBuf) -> Result<ServerConfig> {
	match std::fs::read_to_string(path) {
		Ok(raw) => serde_json::from_str(&raw).context("parsing server config"),
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
		let now = Instant::now();
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
	let mut cfg = load_or_create(&path)?;

	match cli.command {
		Command::Info => {
			print_credentials(&cfg);
			Ok(())
		}
		Command::Run { listen } => {
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
	eprintln!("[libretether-relay] relay listening on udp/{addr}");
	print_credentials(&cfg);

	let relay = Relay::default();
	let secrets = Arc::new((cfg.owner_secret, cfg.agent_secret));
	let conn_limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));

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
				tokio::spawn(async move {
					let _permit = permit; // released when the connection task ends
					if let Err(e) = handle(relay, incoming, &secrets).await {
						eprintln!("[libretether-relay] connection error: {e}");
					}
				});
			}
			_ = shutdown_signal() => {
				eprintln!("[libretether-relay] shutting down");
				break;
			}
		}
	}
	// Tell peers we're going away so they reconnect promptly instead of waiting
	// out the idle timeout, then exit cleanly (so `docker stop` is graceful).
	endpoint.close(0u32.into(), b"relay shutting down");
	Ok(())
}

async fn handle(relay: Relay, incoming: quinn::Incoming, secrets: &(String, String)) -> Result<()> {
	let conn = incoming.accept()?.await?;
	// Bound the auth handshake so a peer that connects and stalls can't hold a
	// connection slot indefinitely.
	let authed = match tokio::time::timeout(HANDSHAKE_TIMEOUT, authenticate(&conn, secrets)).await {
		Ok(Ok(Some(v))) => v,
		Ok(Ok(None)) => return Ok(()), // cleanly rejected (bad secret / proof)
		Ok(Err(e)) => return Err(e),   // I/O error during handshake
		Err(_) => return Ok(()),       // handshake timed out
	};

	match authed.role {
		RelayRole::Controller => serve_controller(relay, conn, authed.send).await,
		RelayRole::Agent => serve_agent(relay, conn, authed.public_key).await,
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
	eprintln!(
		"[libretether-relay] {role_label} connected ({}…)",
		&hello.public_key.chars().take(8).collect::<String>()
	);

	Ok(Some(Authed {
		role: hello.role,
		send,
		public_key: hello.public_key,
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

	// Route each stream the controller opens to the named agent.
	loop {
		let (c_send, mut c_recv) = match conn.accept_bi().await {
			Ok(pair) => pair,
			Err(_) => break,
		};
		let relay = relay.clone();
		tokio::spawn(async move {
			let Ok(route) = read_frame_capped::<_, RouteTo>(&mut c_recv, MAX_CONTROL_FRAME).await else {
				return;
			};
			let Some(agent) = relay.agent(&route.agent) else {
				return;
			};
			if let Ok((a_send, a_recv)) = agent.open_bi().await {
				pipe(c_recv, a_send, a_recv, c_send).await;
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
	Ok(())
}

async fn serve_agent(relay: Relay, conn: quinn::Connection, public_key: String) -> Result<()> {
	let conn_id = conn.stable_id();
	relay.agents.lock().unwrap().insert(public_key.clone(), conn.clone());
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
///
/// Each direction is copied independently and only *its own* send side is
/// finished when its source ends; then we wait for BOTH. Tearing both halves
/// down as soon as one finishes (e.g. `select!`) truncates the reply on a
/// request/response stream — the controller finishes its send half right after
/// the request, so the agent's response would be cut off and surface as
/// "early eof" — and ends live sessions the moment the input half closes.
async fn pipe(mut c_recv: RecvStream, mut a_send: SendStream, mut a_recv: RecvStream, mut c_send: SendStream) {
	let up = async {
		let _ = tokio::io::copy(&mut c_recv, &mut a_send).await;
		let _ = a_send.finish();
	};
	let down = async {
		let _ = tokio::io::copy(&mut a_recv, &mut c_send).await;
		let _ = c_send.finish();
	};
	tokio::join!(up, down);
}

#[cfg(test)]
mod tests {
	use super::*;
	use libretether_protocol::crypto::Identity;
	use std::net::Ipv4Addr;

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
}
