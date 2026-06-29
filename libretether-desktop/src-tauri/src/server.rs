//! The active controller's connection layer. In direct/Tailscale mode it runs a
//! QUIC server agents dial into; in relay mode it dials the relay and learns
//! about agents through presence events. Both paths share [`enroll_and_register`]
//! and address agents through an [`AgentLink`]. All state lives on the
//! [`ActiveController`] passed in, so each controller is fully isolated.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use libretether_protocol::crypto;
use libretether_protocol::frame::{read_frame, read_frame_capped, write_frame, MAX_CONTROL_FRAME};
use libretether_protocol::relay::{RelayAck, RelayChallenge, RelayEvent, RelayHello, RelayProof, RelayRole};
use libretether_protocol::{
	tls, Challenge, ControlRequest, ControlResponse, Hello, HelloAck, SessionGrant, StreamOpen, PROTOCOL_VERSION,
};
use quinn::Endpoint;
use tauri::Emitter;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::link::AgentLink;
use crate::state::{ActiveController, AppState, ControllerKind, LiveConn};

/// Relay-connection progress, surfaced to the connecting screen.
pub const EVENT_RELAY_LOG: &str = "controller:log";
pub const EVENT_RELAY_CONNECTED: &str = "controller:connected";

/// How long the handshake may take before we drop a connecting peer, so one that
/// connects and then stalls can't tie up a task indefinitely.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-registration generation source, so a stale connection's teardown only
/// evicts its own live entry and not a newer reconnection's (see `cleanup`).
static LIVE_GEN: AtomicU64 = AtomicU64::new(1);

fn log(msg: &str) {
	eprintln!("[libretether] {msg}");
}

/// Log a relay-connection line and mirror it to the UI's connecting screen.
fn relay_log(state: &AppState, line: &str) {
	log(line);
	if let Some(app) = state.0.app.get() {
		let _ = app.emit(EVENT_RELAY_LOG, line.to_string());
	}
}

// ---------------------------------------------------------------- direct mode

/// Bind the QUIC listener and accept agents forever (direct / Tailscale mode).
pub async fn serve(state: AppState, ctrl: Arc<ActiveController>) {
	let (cert, key) = match ctrl.profile.cert_key_der() {
		Ok(ck) => ck,
		Err(e) => {
			log(&format!("invalid controller certificate: {e}"));
			return;
		}
	};
	let port = ctrl.profile.kind.listen_port();

	let endpoint = match Endpoint::server(tls::server_config(cert, key), SocketAddr::from(([0, 0, 0, 0], port))) {
		Ok(ep) => ep,
		Err(e) => {
			log(&format!("could not listen on udp/{port}: {e}"));
			return;
		}
	};
	log(&format!("[{}] listening for agents on udp/{port}", ctrl.profile.name));

	while let Some(incoming) = endpoint.accept().await {
		let state = state.clone();
		let ctrl = ctrl.clone();
		tauri::async_runtime::spawn(async move {
			if let Err(e) = handle_direct(state, ctrl, incoming).await {
				log(&format!("connection error: {e}"));
			}
		});
	}
}

async fn handle_direct(state: AppState, ctrl: Arc<ActiveController>, incoming: quinn::Incoming) -> AppResult<()> {
	let conn = incoming
		.accept()
		.map_err(|e| AppError::msg(format!("accept: {e}")))?
		.await
		.map_err(|e| AppError::msg(format!("handshake: {e}")))?;

	let link = AgentLink::direct(conn.clone());
	if let Some((id, generation)) = enroll_and_register(&state, &ctrl, link).await? {
		conn.closed().await;
		cleanup(&state, &ctrl, id, Some(generation));
		log(&format!("agent {id} disconnected"));
	}
	Ok(())
}

// ---------------------------------------------------------------- relay mode

/// Dial the relay and track agents through it (relay mode), reconnecting on drop.
pub async fn serve_relay(state: AppState, ctrl: Arc<ActiveController>) {
	let (relay_addr, owner_secret) = match &ctrl.profile.kind {
		ControllerKind::Relay {
			address, owner_secret, ..
		} => (address.clone(), owner_secret.clone()),
		_ => {
			log("relay serve started for a non-relay controller");
			return;
		}
	};
	let pubkey = ctrl.profile.public_key();

	let mut backoff = 1u64;
	loop {
		match relay_session(&state, &ctrl, &relay_addr, &owner_secret, &pubkey).await {
			Ok(()) => backoff = 1,
			Err(e) => relay_log(&state, &format!("relay error: {e:#}")),
		}
		// The relay is gone, so every agent is unreachable.
		ctrl.live.lock().unwrap().clear();
		state.notify_changed();
		let wait = backoff.min(5);
		relay_log(&state, &format!("reconnecting to relay in {wait}s"));
		tokio::time::sleep(Duration::from_secs(wait)).await;
		backoff = (backoff * 2).min(5);
	}
}

async fn relay_session(
	state: &AppState,
	ctrl: &Arc<ActiveController>,
	relay_addr: &str,
	owner_secret: &str,
	pubkey: &str,
) -> AppResult<()> {
	let addr = tls::resolve(relay_addr)
		.await
		.map_err(|e| AppError::msg(format!("resolving {relay_addr}: {e}")))?;
	relay_log(state, &format!("dialing relay at {addr}"));
	let endpoint = tls::client_endpoint(addr).map_err(|e| AppError::msg(format!("bind client: {e}")))?;
	let conn = endpoint
		.connect(addr, "libretether.local")
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
	// Prove possession of our identity key to the relay.
	let challenge: RelayChallenge = read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await?;
	let proof = RelayProof {
		signature: ctrl.profile.identity()?.sign_b64(challenge.nonce.as_bytes()),
	};
	write_frame(&mut send, &proof).await?;
	let ack: RelayAck = read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await?;
	if !ack.accepted {
		return Err(AppError::msg(format!(
			"relay rejected controller: {}",
			ack.reason.unwrap_or_default()
		)));
	}
	relay_log(state, "connected to relay; awaiting agents");
	if let Some(app) = state.0.app.get() {
		let _ = app.emit(EVENT_RELAY_CONNECTED, ());
	}

	loop {
		let event: RelayEvent = read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await?;
		match event {
			RelayEvent::AgentOnline { public_key } => {
				let state = state.clone();
				let ctrl = ctrl.clone();
				let link = AgentLink::relay(conn.clone(), public_key);
				tauri::async_runtime::spawn(async move {
					if let Err(e) = enroll_and_register(&state, &ctrl, link).await {
						log(&format!("enroll via relay failed: {e}"));
					}
				});
			}
			RelayEvent::AgentOffline { public_key } => {
				let id = ctrl.store.lock().unwrap().id_for_pubkey(&public_key);
				if let Some(id) = id {
					// The relay is authoritative for relay-mode presence, so remove
					// unconditionally (the relay only reports offline once the agent's
					// current connection has actually dropped).
					cleanup(state, ctrl, id, None);
				}
			}
		}
	}
}

// ---------------------------------------------------------------- shared

/// Run the Ed25519 handshake over `link`, enroll/identify the agent, register it
/// in the live map and pull an initial status. Returns the client id and its
/// registration generation on success.
async fn enroll_and_register(
	state: &AppState,
	ctrl: &ActiveController,
	link: AgentLink,
) -> AppResult<Option<(Uuid, u64)>> {
	let (mut send, mut recv) = link.open_bi().await?;
	write_frame(&mut send, &StreamOpen::Handshake).await?;
	let nonce = crypto::random_nonce_b64();
	write_frame(
		&mut send,
		&Challenge {
			nonce: nonce.clone(),
			controller_key: ctrl.profile.public_key(),
		},
	)
	.await?;

	// Read the agent's Hello under a timeout (handshake frames are small), so a
	// peer that connects and then stalls can't tie up this task.
	let hello: Hello =
		match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame_capped(&mut recv, MAX_CONTROL_FRAME)).await {
			Ok(res) => res?,
			Err(_) => return Ok(None),
		};
	if hello.protocol != PROTOCOL_VERSION {
		reject(&mut send, "protocol version mismatch").await;
		return Ok(None);
	}
	if !crypto::verify_b64(&hello.public_key, nonce.as_bytes(), &hello.signature) {
		reject(&mut send, "signature verification failed").await;
		return Ok(None);
	}

	let client_id = {
		let mut store = ctrl.store.lock().unwrap();
		store.authenticate(hello.enrollment_token.as_deref(), &hello.public_key)
	};
	let Some(client_id) = client_id else {
		reject(&mut send, "unknown agent (no matching enrollment token or key)").await;
		return Ok(None);
	};

	// Authenticate ourselves to the agent in turn: sign its nonce with our
	// identity key, which the agent checks against the key it pinned at enrollment.
	let controller_sig = ctrl.profile.identity()?.sign_b64(hello.agent_nonce.as_bytes());
	write_frame(
		&mut send,
		&HelloAck {
			accepted: true,
			reason: None,
			client_id: Some(client_id.to_string()),
			controller_sig,
		},
	)
	.await?;
	let _ = send.finish();

	// Receive the capability token the agent issues for this connection and bind
	// it to the link, so every control/session/tunnel stream we open is stamped
	// with it (the agent rejects streams that aren't).
	let grant: SessionGrant =
		match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame_capped(&mut recv, MAX_CONTROL_FRAME)).await {
			Ok(res) => res?,
			Err(_) => return Ok(None),
		};
	let link = link.with_token(grant.token);

	log(&format!(
		"agent {client_id} connected — {} ({})",
		hello.host.hostname, hello.host.os
	));

	let generation = LIVE_GEN.fetch_add(1, Ordering::Relaxed);
	ctrl.live.lock().unwrap().insert(
		client_id,
		LiveConn {
			link: link.clone(),
			status: None,
			generation,
		},
	);
	state.notify_changed();

	if let Ok(ControlResponse::Status(status)) = control_request(&link, &ControlRequest::Status).await {
		if let Some(live) = ctrl.live.lock().unwrap().get_mut(&client_id) {
			live.status = Some(status);
		}
		state.notify_changed();
	}
	Ok(Some((client_id, generation)))
}

/// Drop a client's live entry and mark it last-seen. `generation` guards the
/// direct-mode teardown: a stale connection only evicts the entry it registered,
/// never a newer reconnection's. Relay mode passes `None` (the relay is
/// authoritative there).
fn cleanup(state: &AppState, ctrl: &ActiveController, id: Uuid, generation: Option<u64>) {
	{
		let mut live = ctrl.live.lock().unwrap();
		let is_current = match generation {
			Some(g) => live.get(&id).map(|c| c.generation) == Some(g),
			None => true,
		};
		if is_current {
			live.remove(&id);
		} else {
			// A newer connection replaced us — leave it in place.
			return;
		}
	}
	// Drop any relay tunnels for this client: they hold the now-defunct
	// connection's capability token, so a reconnect must rebuild them afresh.
	crate::tunnel::close_for(ctrl, id);
	ctrl.store.lock().unwrap().touch_seen(id);
	state.notify_changed();
}

async fn reject(send: &mut quinn::SendStream, reason: &str) {
	let _ = write_frame(
		send,
		&HelloAck {
			accepted: false,
			reason: Some(reason.to_string()),
			client_id: None,
			controller_sig: String::new(),
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
	link.authenticate(&mut send).await?;
	write_frame(&mut send, req).await?;
	let _ = send.finish();
	Ok(read_frame::<_, ControlResponse>(&mut recv).await?)
}
