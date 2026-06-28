//! The controller's connection layer. In direct/Tailscale mode it runs a QUIC
//! server agents dial into; in relay mode it dials the relay and learns about
//! agents through presence events. Both paths share [`enroll_and_register`] and
//! address agents through an [`AgentLink`].

use std::net::SocketAddr;
use std::time::Duration;

use quinn::Endpoint;
use tether_protocol::crypto::{self, Identity};
use tether_protocol::frame::{read_frame, write_frame};
use tether_protocol::relay::{RelayAck, RelayEvent, RelayHello, RelayRole};
use tether_protocol::{tls, Challenge, ControlRequest, ControlResponse, Hello, HelloAck, StreamOpen, PROTOCOL_VERSION};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::link::AgentLink;
use crate::state::{AppState, LiveConn};

fn log(msg: &str) {
	eprintln!("[tether] {msg}");
}

// ---------------------------------------------------------------- direct mode

/// Bind the QUIC listener and accept agents forever (direct / Tailscale mode).
pub async fn serve(state: AppState) {
	let (cert, key, port) = {
		let cfg = state.0.config.lock().unwrap();
		match cfg.cert_key_der() {
			Ok((c, k)) => (c, k, cfg.listen_port),
			Err(e) => {
				log(&format!("invalid controller certificate: {e}"));
				return;
			}
		}
	};

	let endpoint = match Endpoint::server(tls::server_config(cert, key), SocketAddr::from(([0, 0, 0, 0], port))) {
		Ok(ep) => ep,
		Err(e) => {
			log(&format!("could not listen on udp/{port}: {e}"));
			return;
		}
	};
	log(&format!("listening for agents on udp/{port}"));

	while let Some(incoming) = endpoint.accept().await {
		let state = state.clone();
		tauri::async_runtime::spawn(async move {
			if let Err(e) = handle_direct(state, incoming).await {
				log(&format!("connection error: {e}"));
			}
		});
	}
}

async fn handle_direct(state: AppState, incoming: quinn::Incoming) -> AppResult<()> {
	let conn = incoming
		.accept()
		.map_err(|e| AppError::msg(format!("accept: {e}")))?
		.await
		.map_err(|e| AppError::msg(format!("handshake: {e}")))?;

	let link = AgentLink::Direct(conn.clone());
	if let Some(id) = enroll_and_register(state.clone(), link).await? {
		conn.closed().await;
		cleanup(&state, id);
		log(&format!("agent {id} disconnected"));
	}
	Ok(())
}

// ---------------------------------------------------------------- relay mode

/// Dial the relay and track agents through it (relay mode), reconnecting on drop.
pub async fn serve_relay(state: AppState) {
	let (relay_addr, owner_secret, pubkey) = {
		let cfg = state.0.config.lock().unwrap();
		let Some(addr) = cfg.relay().map(str::to_string) else {
			log("relay mode selected but no relay address configured");
			return;
		};
		let pubkey = Identity::from_seed_b64(&cfg.identity_seed)
			.map(|i| i.public_b64())
			.unwrap_or_default();
		(addr, cfg.relay_owner_secret.clone().unwrap_or_default(), pubkey)
	};

	let endpoint = match make_client_endpoint() {
		Ok(ep) => ep,
		Err(e) => {
			log(&format!("could not create relay client: {e}"));
			return;
		}
	};

	let mut backoff = 1u64;
	loop {
		match relay_session(&state, &endpoint, &relay_addr, &owner_secret, &pubkey).await {
			Ok(()) => backoff = 1,
			Err(e) => log(&format!("relay error: {e:#}")),
		}
		// The relay is gone, so every agent is unreachable.
		state.0.live.lock().unwrap().clear();
		state.notify_changed();
		let wait = backoff.min(5);
		log(&format!("reconnecting to relay in {wait}s"));
		tokio::time::sleep(Duration::from_secs(wait)).await;
		backoff = (backoff * 2).min(5);
	}
}

async fn relay_session(
	state: &AppState,
	endpoint: &Endpoint,
	relay_addr: &str,
	owner_secret: &str,
	pubkey: &str,
) -> AppResult<()> {
	let addr = resolve(relay_addr).await?;
	log(&format!("dialing relay at {addr}"));
	let conn = endpoint
		.connect(addr, "tether.local")
		.map_err(|e| AppError::msg(e.to_string()))?
		.await
		.map_err(|e| AppError::msg(format!("relay handshake: {e}")))?;

	let (mut send, mut recv) = conn
		.open_bi()
		.await
		.map_err(|e| AppError::msg(format!("open relay stream: {e}")))?;
	write_frame(
		&mut send,
		&RelayHello {
			role: RelayRole::Controller,
			secret: owner_secret.to_string(),
			public_key: pubkey.to_string(),
		},
	)
	.await?;
	let ack: RelayAck = read_frame(&mut recv).await?;
	if !ack.accepted {
		return Err(AppError::msg(format!(
			"relay rejected controller: {}",
			ack.reason.unwrap_or_default()
		)));
	}
	log("connected to relay; awaiting agents");

	loop {
		let event: RelayEvent = read_frame(&mut recv).await?;
		match event {
			RelayEvent::AgentOnline { public_key } => {
				let state = state.clone();
				let link = AgentLink::Relay {
					relay: conn.clone(),
					agent: public_key,
				};
				tauri::async_runtime::spawn(async move {
					if let Err(e) = enroll_and_register(state, link).await {
						log(&format!("enroll via relay failed: {e}"));
					}
				});
			}
			RelayEvent::AgentOffline { public_key } => {
				let id = state.0.store.lock().unwrap().id_for_pubkey(&public_key);
				if let Some(id) = id {
					cleanup(state, id);
				}
			}
		}
	}
}

fn make_client_endpoint() -> AppResult<Endpoint> {
	let mut endpoint =
		Endpoint::client("0.0.0.0:0".parse().unwrap()).map_err(|e| AppError::msg(format!("bind client: {e}")))?;
	endpoint.set_default_client_config(tls::client_config());
	Ok(endpoint)
}

async fn resolve(addr: &str) -> AppResult<SocketAddr> {
	if let Ok(sa) = addr.parse::<SocketAddr>() {
		return Ok(sa);
	}
	tokio::net::lookup_host(addr)
		.await
		.map_err(|e| AppError::msg(format!("resolving {addr}: {e}")))?
		.next()
		.ok_or_else(|| AppError::msg(format!("no address resolved for {addr}")))
}

// ---------------------------------------------------------------- shared

/// Run the Ed25519 handshake over `link`, enroll/identify the agent, register it
/// in the live map and pull an initial status. Returns the client id on success.
async fn enroll_and_register(state: AppState, link: AgentLink) -> AppResult<Option<Uuid>> {
	let (mut send, mut recv) = link.open_bi().await?;
	write_frame(&mut send, &StreamOpen::Handshake).await?;
	let nonce = crypto::random_nonce_b64();
	write_frame(&mut send, &Challenge { nonce: nonce.clone() }).await?;

	let hello: Hello = read_frame(&mut recv).await?;
	if hello.protocol != PROTOCOL_VERSION {
		reject(&mut send, "protocol version mismatch").await;
		return Ok(None);
	}
	if !crypto::verify_b64(&hello.public_key, nonce.as_bytes(), &hello.signature) {
		reject(&mut send, "signature verification failed").await;
		return Ok(None);
	}

	let client_id = {
		let mut store = state.0.store.lock().unwrap();
		store.authenticate(hello.enrollment_token.as_deref(), &hello.public_key)
	};
	let Some(client_id) = client_id else {
		reject(&mut send, "unknown agent (no matching enrollment token or key)").await;
		return Ok(None);
	};

	write_frame(
		&mut send,
		&HelloAck {
			accepted: true,
			reason: None,
			client_id: Some(client_id.to_string()),
		},
	)
	.await?;
	let _ = send.finish();
	log(&format!(
		"agent {client_id} connected — {} ({})",
		hello.host.hostname, hello.host.os
	));

	state.0.live.lock().unwrap().insert(
		client_id,
		LiveConn {
			link: link.clone(),
			status: None,
		},
	);
	state.notify_changed();

	if let Ok(ControlResponse::Status(status)) = control_request(&link, &ControlRequest::Status).await {
		if let Some(live) = state.0.live.lock().unwrap().get_mut(&client_id) {
			live.status = Some(status);
		}
		state.notify_changed();
	}
	Ok(Some(client_id))
}

fn cleanup(state: &AppState, id: Uuid) {
	state.0.live.lock().unwrap().remove(&id);
	state.0.store.lock().unwrap().touch_seen(id);
	state.notify_changed();
}

async fn reject(send: &mut quinn::SendStream, reason: &str) {
	let _ = write_frame(
		send,
		&HelloAck {
			accepted: false,
			reason: Some(reason.to_string()),
			client_id: None,
		},
	)
	.await;
	let _ = send.finish();
	log(&format!("rejected agent: {reason}"));
}

/// Open a one-shot control stream to an agent, send `req`, and read the response.
pub async fn control_request(link: &AgentLink, req: &ControlRequest) -> AppResult<ControlResponse> {
	let (mut send, mut recv) = link.open_bi().await?;
	write_frame(&mut send, &StreamOpen::Control).await?;
	write_frame(&mut send, req).await?;
	let _ = send.finish();
	Ok(read_frame::<_, ControlResponse>(&mut recv).await?)
}
