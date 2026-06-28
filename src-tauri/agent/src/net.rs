//! The agent's networking core: dial the controller, complete the auth
//! handshake, then service control + session streams until the link drops, with
//! exponential reconnect backoff.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use quinn::{Endpoint, RecvStream, SendStream};
use tether_protocol::frame::{read_frame, write_frame};
use tether_protocol::{tls, Challenge, ControlRequest, Hello, HelloAck, StreamOpen, PROTOCOL_VERSION};

use crate::config::AgentConfig;
use crate::{handlers, host, session};

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Upper bound on the reconnect backoff, so a long-idle agent still retries often.
const RECONNECT_MAX_SECS: u64 = 5;
/// How long a single dial attempt may hang before we give up and retry.
const CONNECT_TIMEOUT_SECS: u64 = 8;

pub fn log(msg: &str) {
	eprintln!("[tether-agent] {msg}");
}

/// Load config and run the connect/serve loop forever.
pub async fn run(cfg_path: PathBuf) -> Result<()> {
	let mut cfg = AgentConfig::load(&cfg_path)?;
	handlers::mark_start();
	let endpoint = make_endpoint()?;
	log(&format!(
		"agent {AGENT_VERSION} starting; controller = {}",
		cfg.controller_addr
	));

	let mut backoff = 1u64;
	loop {
		match connect_once(&endpoint, &mut cfg, &cfg_path).await {
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

fn make_endpoint() -> Result<Endpoint> {
	let mut endpoint = Endpoint::client("0.0.0.0:0".parse().unwrap()).context("binding client socket")?;
	endpoint.set_default_client_config(tls::client_config());
	Ok(endpoint)
}

async fn connect_once(endpoint: &Endpoint, cfg: &mut AgentConfig, cfg_path: &PathBuf) -> Result<()> {
	let addr = resolve(&cfg.controller_addr).await?;
	log(&format!("dialing controller at {addr}"));
	let connecting = endpoint.connect(addr, &cfg.server_name)?;
	let conn = tokio::time::timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), connecting)
		.await
		.map_err(|_| anyhow!("dial timed out after {CONNECT_TIMEOUT_SECS}s"))?
		.context("quic handshake")?;
	log("connected; awaiting challenge");

	// Handshake stream is opened by the controller.
	let (mut send, mut recv) = conn.accept_bi().await.context("accept handshake stream")?;
	match read_frame::<_, StreamOpen>(&mut recv).await? {
		StreamOpen::Handshake => {}
		other => return Err(anyhow!("expected handshake stream, got {other:?}")),
	}
	let challenge: Challenge = read_frame(&mut recv).await.context("reading challenge")?;

	let identity = cfg.identity()?;
	let hello = Hello {
		protocol: PROTOCOL_VERSION,
		enrollment_token: cfg.enrollment_token.clone(),
		public_key: identity.public_b64(),
		signature: identity.sign_b64(challenge.nonce.as_bytes()),
		host: host::host_info(),
		agent_version: AGENT_VERSION.to_string(),
	};
	write_frame(&mut send, &hello).await.context("sending hello")?;

	let ack: HelloAck = read_frame(&mut recv).await.context("reading ack")?;
	if !ack.accepted {
		return Err(anyhow!("controller rejected agent: {}", ack.reason.unwrap_or_default()));
	}
	log(&format!(
		"authenticated as client {}",
		ack.client_id.clone().unwrap_or_default()
	));

	// We're enrolled — burn the one-time token so future runs use only the key.
	if cfg.enrollment_token.is_some() {
		cfg.enrollment_token = None;
		cfg.client_id = ack.client_id.clone();
		if let Err(e) = cfg.save(cfg_path) {
			log(&format!("warning: could not persist enrolled config: {e}"));
		}
	}
	let _ = send.finish();

	// Serve control + session streams until the connection ends.
	loop {
		let (send, recv) = conn.accept_bi().await.map_err(|e| anyhow!("connection ended: {e}"))?;
		tokio::spawn(handle_stream(send, recv));
	}
}

async fn handle_stream(mut send: SendStream, mut recv: RecvStream) {
	let open = match read_frame::<_, StreamOpen>(&mut recv).await {
		Ok(o) => o,
		Err(_) => return,
	};
	match open {
		StreamOpen::Control => {
			let req: ControlRequest = match read_frame(&mut recv).await {
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
		StreamOpen::Handshake => log("unexpected handshake stream after auth"),
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
