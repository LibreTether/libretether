//! The controller's QUIC server. Agents dial in here; we challenge them, verify
//! the Ed25519 signature, enroll/identify them, and track the live connection.

use std::net::SocketAddr;

use quinn::Endpoint;
use tether_protocol::crypto;
use tether_protocol::frame::{read_frame, write_frame};
use tether_protocol::{tls, Challenge, ControlRequest, ControlResponse, Hello, HelloAck, StreamOpen, PROTOCOL_VERSION};

use crate::error::{AppError, AppResult};
use crate::state::{AppState, LiveConn};

fn log(msg: &str) {
	eprintln!("[tether] {msg}");
}

/// Bind the QUIC listener and accept agents forever. Runs as a background task.
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
			if let Err(e) = handle_connection(state, incoming).await {
				log(&format!("connection error: {e}"));
			}
		});
	}
}

async fn handle_connection(state: AppState, incoming: quinn::Incoming) -> AppResult<()> {
	let conn = incoming
		.accept()
		.map_err(|e| AppError::msg(format!("accept: {e}")))?
		.await
		.map_err(|e| AppError::msg(format!("handshake: {e}")))?;

	// Controller opens the handshake stream and challenges the agent.
	let (mut send, mut recv) = conn
		.open_bi()
		.await
		.map_err(|e| AppError::msg(format!("open handshake stream: {e}")))?;
	write_frame(&mut send, &StreamOpen::Handshake).await?;
	let nonce = crypto::random_nonce_b64();
	write_frame(&mut send, &Challenge { nonce: nonce.clone() }).await?;

	let hello: Hello = read_frame(&mut recv).await?;
	if hello.protocol != PROTOCOL_VERSION {
		reject(&mut send, "protocol version mismatch").await;
		return Ok(());
	}
	if !crypto::verify_b64(&hello.public_key, nonce.as_bytes(), &hello.signature) {
		reject(&mut send, "signature verification failed").await;
		return Ok(());
	}

	let client_id = {
		let mut store = state.0.store.lock().unwrap();
		store.authenticate(hello.enrollment_token.as_deref(), &hello.public_key)
	};
	let Some(client_id) = client_id else {
		reject(&mut send, "unknown agent (no matching enrollment token or key)").await;
		return Ok(());
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
			conn: conn.clone(),
			status: None,
		},
	);
	state.notify_changed();

	// Pull an initial status snapshot so the UI has something to show.
	if let Ok(ControlResponse::Status(status)) = control_request(&conn, &ControlRequest::Status).await {
		if let Some(live) = state.0.live.lock().unwrap().get_mut(&client_id) {
			live.status = Some(status);
		}
		state.notify_changed();
	}

	// Block until the agent goes away, then clean up.
	conn.closed().await;
	state.0.live.lock().unwrap().remove(&client_id);
	state.0.store.lock().unwrap().touch_seen(client_id);
	state.notify_changed();
	log(&format!("agent {client_id} disconnected"));
	Ok(())
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

/// Open a one-shot control stream, send `req`, and read the response.
pub async fn control_request(conn: &quinn::Connection, req: &ControlRequest) -> AppResult<ControlResponse> {
	let (mut send, mut recv) = conn
		.open_bi()
		.await
		.map_err(|e| AppError::msg(format!("open control stream: {e}")))?;
	write_frame(&mut send, &StreamOpen::Control).await?;
	write_frame(&mut send, req).await?;
	let _ = send.finish();
	Ok(read_frame::<_, ControlResponse>(&mut recv).await?)
}
