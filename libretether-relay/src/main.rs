//! LibreTether relay (`libretether-relay`).
//!
//! Run this on a public cloud host. The controller and the agents all dial out
//! to it; it authenticates each side (owner secret vs agent secret), tracks
//! agents by Ed25519 public key, and pipes streams between the controller and
//! the addressed agent. It never inspects stream contents — the LibreTether handshake,
//! control RPCs, live session and TCP tunnels are all end-to-end.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use clap::{Parser, Subcommand};
use libretether_protocol::crypto::{self, random_alnum};
use libretether_protocol::frame::{read_frame_capped, write_frame, MAX_CONTROL_FRAME};
use libretether_protocol::relay::{RelayAck, RelayChallenge, RelayEvent, RelayHello, RelayProof, RelayRole, RouteTo};
use libretether_protocol::{secret, tls, DEFAULT_PORT};
use quinn::{Endpoint, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

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

/// The single connected controller's event sender, tagged with its generation.
type ControllerSlot = Arc<Mutex<Option<(u64, UnboundedSender<RelayEvent>)>>>;

#[derive(Clone, Default)]
struct Relay {
	agents: Arc<Mutex<HashMap<String, quinn::Connection>>>,
	controller: ControllerSlot,
}

impl Relay {
	fn agent(&self, public_key: &str) -> Option<quinn::Connection> {
		self.agents.lock().unwrap().get(public_key).cloned()
	}

	fn notify(&self, event: RelayEvent) {
		if let Some((_, tx)) = self.controller.lock().unwrap().as_ref() {
			let _ = tx.send(event);
		}
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

	loop {
		tokio::select! {
			incoming = endpoint.accept() => {
				let Some(incoming) = incoming else { break };
				let relay = relay.clone();
				let secrets = secrets.clone();
				tokio::spawn(async move {
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

/// Resolve on the first SIGINT/SIGTERM (Ctrl+C on Windows) so `docker stop`
/// ends the relay gracefully instead of timing out into a SIGKILL.
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

async fn handle(relay: Relay, incoming: quinn::Incoming, secrets: &(String, String)) -> Result<()> {
	let conn = incoming.accept()?.await?;
	let (mut send, mut recv) = conn.accept_bi().await.context("accept hello stream")?;
	let hello: RelayHello = read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await?;

	let (expected, role_ok) = match hello.role {
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
		return Ok(());
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
		return Ok(());
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
		"[libretether-relay] {role_ok} connected ({}…)",
		&hello.public_key.chars().take(8).collect::<String>()
	);

	match hello.role {
		RelayRole::Controller => serve_controller(relay, conn, send).await,
		RelayRole::Agent => serve_agent(relay, conn, hello.public_key).await,
	}
}

/// The controller pushes presence events out on `events`, and opens one routed
/// bi stream per request which we pipe to the addressed agent.
async fn serve_controller(relay: Relay, conn: quinn::Connection, mut events: SendStream) -> Result<()> {
	let generation = CONTROLLER_GEN.fetch_add(1, Ordering::Relaxed);
	let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RelayEvent>();

	// Announce agents that are already connected, then install the sender.
	{
		let agents = relay.agents.lock().unwrap();
		for key in agents.keys() {
			let _ = tx.send(RelayEvent::AgentOnline {
				public_key: key.clone(),
			});
		}
	}
	*relay.controller.lock().unwrap() = Some((generation, tx));

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
		if slot.as_ref().map(|(g, _)| *g) == Some(generation) {
			slot.take();
		}
	}
	events_task.abort();
	Ok(())
}

async fn serve_agent(relay: Relay, conn: quinn::Connection, public_key: String) -> Result<()> {
	relay.agents.lock().unwrap().insert(public_key.clone(), conn.clone());
	relay.notify(RelayEvent::AgentOnline {
		public_key: public_key.clone(),
	});

	conn.closed().await;

	relay.agents.lock().unwrap().remove(&public_key);
	relay.notify(RelayEvent::AgentOffline { public_key });
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
