//! Controller side of a live screen-control session. Bridges a QUIC session
//! stream to the webview: agent frames are emitted as Tauri events, and input
//! from the UI is pushed back to the agent.

use tauri::Emitter;
use tether_protocol::frame::{read_frame, write_frame};
use tether_protocol::{InputEvent, SessionClient, SessionConfig, SessionServer, StreamOpen};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::state::{AppState, SessionHandle};

/// Open a session to an online agent and start relaying frames to the UI.
/// Idempotent: if a session is already running for `id`, this is a no-op.
pub async fn start(state: AppState, id: Uuid, cfg: SessionConfig) -> AppResult<()> {
	if state.0.sessions.lock().unwrap().contains_key(&id) {
		return Ok(());
	}
	let conn = state.connection(id).ok_or(AppError::Offline)?;
	let (mut send, mut recv) = conn
		.open_bi()
		.await
		.map_err(|e| AppError::msg(format!("open session stream: {e}")))?;
	write_frame(&mut send, &StreamOpen::Session).await?;
	write_frame(&mut send, &SessionClient::Start(cfg)).await?;

	let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<SessionClient>();
	let app = state.0.app.get().cloned();
	let state_task = state.clone();

	let task = tauri::async_runtime::spawn(async move {
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
		state_task.0.sessions.lock().unwrap().remove(&id);
		emit(&app, &format!("session:closed:{id}"), ());
	});

	state
		.0
		.sessions
		.lock()
		.unwrap()
		.insert(id, SessionHandle { input_tx, task });
	Ok(())
}

/// Push an input event into a running session.
pub fn send_input(state: &AppState, id: Uuid, event: InputEvent) -> AppResult<()> {
	let sessions = state.0.sessions.lock().unwrap();
	let handle = sessions
		.get(&id)
		.ok_or_else(|| AppError::msg("no active session for that client"))?;
	handle
		.input_tx
		.send(SessionClient::Input(event))
		.map_err(|_| AppError::msg("session has closed"))
}

/// Stop a running session, if any.
pub fn stop(state: &AppState, id: Uuid) {
	if let Some(handle) = state.0.sessions.lock().unwrap().remove(&id) {
		let _ = handle.input_tx.send(SessionClient::Stop);
		handle.task.abort();
	}
}

fn emit<P: serde::Serialize + Clone>(app: &Option<tauri::AppHandle>, event: &str, payload: P) {
	if let Some(app) = app {
		let _ = app.emit(event, payload);
	}
}
