//! Tauri commands — the surface the React UI calls into.

use serde::Serialize;
use tauri::State;
use tether_protocol::{
	AgentStatus, ControlRequest, ControlResponse, ExecResult, InputEvent, ScreenshotResult, SessionConfig,
};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::registry::{Client, ClientOs};
use crate::server::control_request;
use crate::state::AppState;
use crate::tailscale::{self, TailscaleInfo};
use crate::{deploy, session};

#[derive(Serialize)]
pub struct ClientDto {
	pub id: String,
	pub name: String,
	pub os: String,
	pub created_at: u64,
	pub enrolled: bool,
	pub online: bool,
	pub last_seen: Option<u64>,
	pub status: Option<AgentStatus>,
}

#[derive(Serialize)]
pub struct CreateClientResult {
	pub client: ClientDto,
	pub deploy_script: String,
}

#[derive(Serialize)]
pub struct ControllerInfo {
	pub listen_port: u16,
	pub fingerprint: String,
	pub tailscale: TailscaleInfo,
	/// Manual override for the address agents dial (empty = use Tailscale).
	pub advertise_addr: Option<String>,
	/// Tailscale pre-auth key embedded in deploy scripts (empty = direct mode).
	pub tailscale_auth_key: Option<String>,
}

fn to_dto(client: &Client, online: bool, status: Option<AgentStatus>) -> ClientDto {
	ClientDto {
		id: client.id.to_string(),
		name: client.name.clone(),
		os: client.os.as_str().to_string(),
		created_at: client.created_at,
		enrolled: client.enrolled,
		online,
		last_seen: client.last_seen,
		status,
	}
}

fn parse_id(id: &str) -> AppResult<Uuid> {
	Uuid::parse_str(id).map_err(|_| AppError::msg("invalid client id"))
}

/// Build the `host:port` agents should dial: the manual advertise address if
/// set, otherwise the controller's Tailscale address.
async fn controller_addr(state: &AppState) -> String {
	let (override_addr, port) = {
		let cfg = state.0.config.lock().unwrap();
		(
			cfg.advertise_addr.clone().filter(|s| !s.trim().is_empty()),
			cfg.listen_port,
		)
	};
	if let Some(addr) = override_addr {
		return with_port(&addr, port);
	}
	match tailscale::status().await.address {
		Some(addr) => format!("{addr}:{port}"),
		None => format!("<controller-address>:{port}"),
	}
}

fn with_port(addr: &str, port: u16) -> String {
	if addr.contains(':') {
		addr.to_string()
	} else {
		format!("{addr}:{port}")
	}
}

/// The configured Tailscale pre-auth key (embedded in deploy scripts), if any.
fn auth_key(state: &AppState) -> Option<String> {
	state
		.0
		.config
		.lock()
		.unwrap()
		.tailscale_auth_key
		.clone()
		.filter(|s| !s.trim().is_empty())
}

// ---------------------------------------------------------------- registry

#[tauri::command]
pub async fn list_clients(state: State<'_, AppState>) -> AppResult<Vec<ClientDto>> {
	let state = state.inner().clone();
	let clients: Vec<Client> = state.0.store.lock().unwrap().list().to_vec();
	let live = state.0.live.lock().unwrap();
	Ok(clients
		.iter()
		.map(|c| {
			let conn = live.get(&c.id);
			to_dto(c, conn.is_some(), conn.and_then(|l| l.status.clone()))
		})
		.collect())
}

#[tauri::command]
pub async fn create_client(state: State<'_, AppState>, name: String, os: ClientOs) -> AppResult<CreateClientResult> {
	let state = state.inner().clone();
	let name = name.trim().to_string();
	if name.is_empty() {
		return Err(AppError::msg("name cannot be empty"));
	}
	let client = state.0.store.lock().unwrap().create(name, os)?;
	let addr = controller_addr(&state).await;
	let token = client.enrollment_token.clone().unwrap_or_default();
	let deploy_script = deploy::script(&client.name, client.os, &addr, &token, auth_key(&state).as_deref());
	state.notify_changed();
	Ok(CreateClientResult {
		client: to_dto(&client, false, None),
		deploy_script,
	})
}

#[tauri::command]
pub async fn remove_client(state: State<'_, AppState>, id: String) -> AppResult<()> {
	let state = state.inner().clone();
	let id = parse_id(&id)?;
	session::stop(&state, id);
	if let Some(conn) = state.connection(id) {
		conn.close(0u32.into(), b"removed");
	}
	state.0.live.lock().unwrap().remove(&id);
	state.0.store.lock().unwrap().remove(id)?;
	state.notify_changed();
	Ok(())
}

#[tauri::command]
pub async fn rename_client(state: State<'_, AppState>, id: String, name: String) -> AppResult<()> {
	let state = state.inner().clone();
	let id = parse_id(&id)?;
	let name = name.trim().to_string();
	if name.is_empty() {
		return Err(AppError::msg("name cannot be empty"));
	}
	state.0.store.lock().unwrap().rename(id, name)?;
	state.notify_changed();
	Ok(())
}

#[tauri::command]
pub async fn get_deploy_script(state: State<'_, AppState>, id: String, os: Option<ClientOs>) -> AppResult<String> {
	let state = state.inner().clone();
	let id = parse_id(&id)?;
	let (name, client_os, token) = {
		let store = state.0.store.lock().unwrap();
		let c = store.get(id).ok_or(AppError::NotFound)?;
		(c.name.clone(), c.os, c.enrollment_token.clone())
	};
	let token = token.ok_or_else(|| AppError::msg("client already enrolled — reset its token to re-deploy"))?;
	let addr = controller_addr(&state).await;
	Ok(deploy::script(
		&name,
		os.unwrap_or(client_os),
		&addr,
		&token,
		auth_key(&state).as_deref(),
	))
}

#[tauri::command]
pub async fn reset_token(state: State<'_, AppState>, id: String) -> AppResult<CreateClientResult> {
	let state = state.inner().clone();
	let id = parse_id(&id)?;
	let token = state.0.store.lock().unwrap().reset_token(id)?;
	// Resolve the controller address with no lock held across the await.
	let addr = controller_addr(&state).await;
	let client = {
		let store = state.0.store.lock().unwrap();
		store.get(id).ok_or(AppError::NotFound)?.clone()
	};
	let deploy_script = deploy::script(&client.name, client.os, &addr, &token, auth_key(&state).as_deref());
	state.notify_changed();
	Ok(CreateClientResult {
		client: to_dto(&client, state.is_online(id), None),
		deploy_script,
	})
}

// ---------------------------------------------------------------- live control

#[tauri::command]
pub async fn client_status(state: State<'_, AppState>, id: String) -> AppResult<AgentStatus> {
	let state = state.inner().clone();
	let id = parse_id(&id)?;
	let conn = state.connection(id).ok_or(AppError::Offline)?;
	match control_request(&conn, &ControlRequest::Status).await? {
		ControlResponse::Status(status) => {
			if let Some(live) = state.0.live.lock().unwrap().get_mut(&id) {
				live.status = Some(status.clone());
			}
			Ok(status)
		}
		ControlResponse::Error { message } => Err(AppError::Agent(message)),
		_ => Err(AppError::msg("unexpected response")),
	}
}

#[tauri::command]
pub async fn client_exec(
	state: State<'_, AppState>,
	id: String,
	program: String,
	args: Vec<String>,
	timeout_secs: Option<u64>,
) -> AppResult<ExecResult> {
	let state = state.inner().clone();
	let id = parse_id(&id)?;
	let conn = state.connection(id).ok_or(AppError::Offline)?;
	match control_request(
		&conn,
		&ControlRequest::Exec {
			program,
			args,
			timeout_secs,
		},
	)
	.await?
	{
		ControlResponse::Exec(result) => Ok(result),
		ControlResponse::Error { message } => Err(AppError::Agent(message)),
		_ => Err(AppError::msg("unexpected response")),
	}
}

#[tauri::command]
pub async fn client_screenshot(
	state: State<'_, AppState>,
	id: String,
	display: Option<u32>,
) -> AppResult<ScreenshotResult> {
	let state = state.inner().clone();
	let id = parse_id(&id)?;
	let conn = state.connection(id).ok_or(AppError::Offline)?;
	match control_request(&conn, &ControlRequest::Screenshot { display }).await? {
		ControlResponse::Screenshot(shot) => Ok(shot),
		ControlResponse::Error { message } => Err(AppError::Agent(message)),
		_ => Err(AppError::msg("unexpected response")),
	}
}

// ---------------------------------------------------------------- session

#[tauri::command]
pub async fn start_control(
	state: State<'_, AppState>,
	id: String,
	display: Option<u32>,
	quality: Option<u8>,
	max_fps: Option<u8>,
) -> AppResult<()> {
	let state = state.inner().clone();
	let id = parse_id(&id)?;
	let defaults = SessionConfig::default();
	let cfg = SessionConfig {
		display: display.unwrap_or(defaults.display),
		quality: quality.unwrap_or(defaults.quality),
		max_fps: max_fps.unwrap_or(defaults.max_fps),
	};
	session::start(state, id, cfg);
	Ok(())
}

#[tauri::command]
pub async fn send_input(state: State<'_, AppState>, id: String, event: InputEvent) -> AppResult<()> {
	let state = state.inner().clone();
	let id = parse_id(&id)?;
	session::send_input(&state, id, event)
}

#[tauri::command]
pub async fn stop_control(state: State<'_, AppState>, id: String) -> AppResult<()> {
	let state = state.inner().clone();
	let id = parse_id(&id)?;
	session::stop(&state, id);
	Ok(())
}

// ---------------------------------------------------------------- controller

/// Write text to a path the user picked via the save dialog. Used to save a
/// generated deploy script; `.sh` files are made executable.
#[tauri::command]
pub async fn save_text_file(path: String, contents: String) -> AppResult<()> {
	std::fs::write(&path, contents)?;
	#[cfg(unix)]
	if path.ends_with(".sh") {
		use std::os::unix::fs::PermissionsExt;
		let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
	}
	Ok(())
}

#[tauri::command]
pub async fn controller_info(state: State<'_, AppState>) -> AppResult<ControllerInfo> {
	let state = state.inner().clone();
	let (listen_port, fingerprint, advertise_addr, tailscale_auth_key) = {
		let cfg = state.0.config.lock().unwrap();
		(
			cfg.listen_port,
			cfg.fingerprint(),
			cfg.advertise_addr.clone(),
			cfg.tailscale_auth_key.clone(),
		)
	};
	Ok(ControllerInfo {
		listen_port,
		fingerprint,
		tailscale: tailscale::status().await,
		advertise_addr,
		tailscale_auth_key,
	})
}

/// Update the address agents dial and/or the embedded Tailscale auth key. Empty
/// strings clear the setting.
#[tauri::command]
pub async fn set_controller_settings(
	state: State<'_, AppState>,
	advertise_addr: Option<String>,
	tailscale_auth_key: Option<String>,
) -> AppResult<()> {
	let state = state.inner().clone();
	{
		let mut cfg = state.0.config.lock().unwrap();
		cfg.advertise_addr = advertise_addr.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
		cfg.tailscale_auth_key = tailscale_auth_key
			.map(|s| s.trim().to_string())
			.filter(|s| !s.is_empty());
	}
	state.save_config()?;
	state.notify_changed();
	Ok(())
}
