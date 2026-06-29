//! Controller side of a live screen-control session. Bridges a QUIC session
//! stream to the webview: agent frames are emitted as Tauri events, and input
//! from the UI is pushed back to the agent.
//!
//! Sessions live on the [`ActiveController`], so they're torn down when the
//! controller is exited. Starting a session is synchronous and stop-first: any
//! existing session is torn down and the new handle registered before any
//! `.await`, so rapid start/stop/start sequences can't leave two racing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use libretether_protocol::frame::{read_frame, write_frame};
use libretether_protocol::{InputEvent, SessionClient, SessionConfig, SessionServer, StreamOpen};
use tauri::{AppHandle, Emitter};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::{ActiveController, AppState, SessionHandle};

static SESSION_GEN: AtomicU64 = AtomicU64::new(1);

/// Start (or restart) a live session for `id` on the active controller.
pub fn start(state: &AppState, ctrl: Arc<ActiveController>, id: Uuid, cfg: SessionConfig) {
	stop(&ctrl, id);

	let token = SESSION_GEN.fetch_add(1, Ordering::Relaxed);
	let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<SessionClient>();
	let app = state.0.app.get().cloned();
	let task = tauri::async_runtime::spawn(drive(ctrl.clone(), id, token, cfg, input_rx, app));
	ctrl.sessions
		.lock()
		.unwrap()
		.insert(id, SessionHandle { input_tx, task, token });
}

async fn drive(
	ctrl: Arc<ActiveController>,
	id: Uuid,
	token: u64,
	cfg: SessionConfig,
	mut input_rx: tokio::sync::mpsc::UnboundedReceiver<SessionClient>,
	app: Option<AppHandle>,
) {
	let Some(conn) = ctrl.connection(id) else {
		emit(&app, &format!("session:error:{id}"), "client is offline".to_string());
		finish(&ctrl, id, token, &app);
		return;
	};
	let (mut send, mut recv) = match conn.open_authenticated(StreamOpen::Session).await {
		Ok(pair) => pair,
		Err(e) => {
			emit(
				&app,
				&format!("session:error:{id}"),
				format!("could not open session: {e}"),
			);
			finish(&ctrl, id, token, &app);
			return;
		}
	};
	if write_frame(&mut send, &SessionClient::Start(cfg)).await.is_err() {
		finish(&ctrl, id, token, &app);
		return;
	}

	// Writer: forward UI input/control to the agent.
	let writer = tauri::async_runtime::spawn(async move {
		while let Some(msg) = input_rx.recv().await {
			let stop = matches!(msg, SessionClient::Stop);
			if write_frame(&mut send, &msg).await.is_err() {
				break;
			}
			if stop {
				let _ = send.finish();
				break;
			}
		}
	});

	// Reader: relay agent frames/metadata to the webview.
	loop {
		match read_frame::<_, SessionServer>(&mut recv).await {
			Ok(SessionServer::Frame(frame)) => emit(&app, &format!("session:frame:{id}"), frame),
			Ok(SessionServer::Meta { display, width, height }) => emit(
				&app,
				&format!("session:meta:{id}"),
				serde_json::json!({ "display": display, "width": width, "height": height }),
			),
			Ok(SessionServer::Error { message }) => {
				emit(&app, &format!("session:error:{id}"), message);
				break;
			}
			Err(_) => break,
		}
	}

	writer.abort();
	finish(&ctrl, id, token, &app);
}

/// Remove our handle and notify the UI — but only if we're still the current
/// session (a newer one may have replaced us).
fn finish(ctrl: &ActiveController, id: Uuid, token: u64, app: &Option<AppHandle>) {
	let mut sessions = ctrl.sessions.lock().unwrap();
	if sessions.get(&id).map(|h| h.token) == Some(token) {
		sessions.remove(&id);
		drop(sessions);
		emit(app, &format!("session:closed:{id}"), ());
	}
}

/// Push an input event into a running session.
pub fn send_input(ctrl: &ActiveController, id: Uuid, event: InputEvent) -> AppResult<()> {
	let sessions = ctrl.sessions.lock().unwrap();
	let handle = sessions
		.get(&id)
		.ok_or_else(|| AppError::msg("no active session for that client"))?;
	handle
		.input_tx
		.send(SessionClient::Input(event))
		.map_err(|_| AppError::msg("session has closed"))
}

/// Stop a running session, if any.
pub fn stop(ctrl: &ActiveController, id: Uuid) {
	if let Some(handle) = ctrl.sessions.lock().unwrap().remove(&id) {
		let _ = handle.input_tx.send(SessionClient::Stop);
		handle.task.abort();
	}
}

fn emit<P: serde::Serialize + Clone>(app: &Option<AppHandle>, event: &str, payload: P) {
	if let Some(app) = app {
		let _ = app.emit(event, payload);
	}
}
