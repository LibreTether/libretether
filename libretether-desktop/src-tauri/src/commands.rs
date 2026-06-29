//! Tauri commands — the surface the React UI calls into. Controller-management
//! commands operate on saved profiles; everything else operates on the single
//! currently-active controller (and errors if none is connected).

use libretether_protocol::{
	AgentStatus, ControlRequest, ControlResponse, ExecResult, InputEvent, ScreenshotResult, SessionConfig, DEFAULT_PORT,
};
use serde::Serialize;
use tauri::State;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::link::AgentLink;
use crate::registry::{Client, ClientOs};
use crate::server::control_request;
use crate::state::{ActiveController, AppState, ControllerKind};
use crate::tailscale::{self, TailscaleInfo};
use crate::{deploy, session};

// ---------------------------------------------------------------- DTOs

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

/// A saved controller, for the selection screen. Includes the full `kind` (with
/// secrets) so the edit form can pre-fill — this is the user's own local config.
#[derive(Serialize)]
pub struct ControllerSummary {
	pub id: String,
	pub name: String,
	pub kind: ControllerKind,
	pub fingerprint: String,
	pub machine_count: usize,
	pub active: bool,
}

/// Info about the controller that is currently connected.
#[derive(Serialize)]
pub struct ActiveInfo {
	pub id: String,
	pub name: String,
	pub kind: ControllerKind,
	pub fingerprint: String,
	/// host:port agents dial (direct/tailscale), or the relay address.
	pub reachable_at: Option<String>,
	/// Present only for Tailscale controllers.
	pub tailscale: Option<TailscaleInfo>,
}

#[derive(Serialize)]
pub struct SettingsDto {
	pub rdp_client: Option<String>,
	pub terminal: Option<String>,
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
	Uuid::parse_str(id).map_err(|_| AppError::msg("invalid id"))
}

fn with_port(addr: &str, port: u16) -> String {
	if addr.contains(':') {
		addr.to_string()
	} else {
		format!("{addr}:{port}")
	}
}

async fn tailscale_addr(port: u16) -> String {
	match tailscale::status().await.address {
		Some(addr) => format!("{addr}:{port}"),
		None => format!("<controller-address>:{port}"),
	}
}

async fn direct_addr(advertise: Option<String>, port: u16) -> String {
	match advertise.filter(|s| !s.trim().is_empty()) {
		Some(addr) => with_port(&addr, port),
		None => format!("<controller-address>:{port}"),
	}
}

/// Build the deploy target for the active controller from its kind.
async fn deploy_target(ctrl: &ActiveController) -> deploy::DeployTarget {
	match ctrl.profile.kind.clone() {
		ControllerKind::Relay {
			address, agent_secret, ..
		} => deploy::DeployTarget::Relay {
			address: with_port(&address, DEFAULT_PORT),
			agent_secret,
		},
		ControllerKind::Direct {
			advertise_addr,
			listen_port,
		} => deploy::DeployTarget::Controller {
			address: direct_addr(advertise_addr, listen_port).await,
			auth_key: None,
		},
		ControllerKind::Tailscale { auth_key, listen_port } => deploy::DeployTarget::Controller {
			address: tailscale_addr(listen_port).await,
			auth_key: auth_key.filter(|s| !s.trim().is_empty()),
		},
	}
}

// ---------------------------------------------------------------- controllers

fn summary(state: &AppState, profile: crate::state::ControllerProfile, active_id: Option<Uuid>) -> ControllerSummary {
	ControllerSummary {
		machine_count: state.machine_count(profile.id),
		active: active_id == Some(profile.id),
		fingerprint: profile.fingerprint(),
		id: profile.id.to_string(),
		name: profile.name,
		kind: profile.kind,
	}
}

#[tauri::command]
pub async fn list_controllers(state: State<'_, AppState>) -> AppResult<Vec<ControllerSummary>> {
	let state = state.inner().clone();
	let active_id = state.active().map(|c| c.profile.id);
	Ok(state
		.list_profiles()?
		.into_iter()
		.map(|p| summary(&state, p, active_id))
		.collect())
}

#[tauri::command]
pub async fn create_controller(
	state: State<'_, AppState>,
	name: String,
	kind: ControllerKind,
) -> AppResult<ControllerSummary> {
	let state = state.inner().clone();
	let name = name.trim().to_string();
	if name.is_empty() {
		return Err(AppError::msg("name cannot be empty"));
	}
	let profile = state.create_profile(name, kind)?;
	Ok(summary(&state, profile, state.active().map(|c| c.profile.id)))
}

#[tauri::command]
pub async fn update_controller(
	state: State<'_, AppState>,
	id: String,
	name: String,
	kind: ControllerKind,
) -> AppResult<ControllerSummary> {
	let state = state.inner().clone();
	let id = parse_id(&id)?;
	let name = name.trim().to_string();
	if name.is_empty() {
		return Err(AppError::msg("name cannot be empty"));
	}
	let profile = state.update_profile(id, name, kind)?;
	Ok(summary(&state, profile, state.active().map(|c| c.profile.id)))
}

#[tauri::command]
pub async fn delete_controller(state: State<'_, AppState>, id: String) -> AppResult<()> {
	let state = state.inner().clone();
	state.delete_profile(parse_id(&id)?)
}

#[tauri::command]
pub async fn select_controller(state: State<'_, AppState>, id: String) -> AppResult<ActiveInfo> {
	let state = state.inner().clone();
	let ctrl = state.activate(parse_id(&id)?)?;
	Ok(active_info(&ctrl).await)
}

#[tauri::command]
pub async fn exit_controller(state: State<'_, AppState>) -> AppResult<()> {
	state.inner().deactivate();
	Ok(())
}

#[tauri::command]
pub async fn active_controller(state: State<'_, AppState>) -> AppResult<Option<ActiveInfo>> {
	let state = state.inner().clone();
	match state.active() {
		Some(ctrl) => Ok(Some(active_info(&ctrl).await)),
		None => Ok(None),
	}
}

async fn active_info(ctrl: &ActiveController) -> ActiveInfo {
	let p = &ctrl.profile;
	let (reachable_at, tailscale) = match &p.kind {
		ControllerKind::Relay { address, .. } => (Some(with_port(address, DEFAULT_PORT)), None),
		ControllerKind::Direct {
			advertise_addr,
			listen_port,
		} => {
			let addr = advertise_addr
				.clone()
				.filter(|s| !s.trim().is_empty())
				.map(|a| with_port(&a, *listen_port));
			(addr, None)
		}
		ControllerKind::Tailscale { listen_port, .. } => {
			let ts = tailscale::status().await;
			let addr = ts.address.as_ref().map(|a| format!("{a}:{listen_port}"));
			(addr, Some(ts))
		}
	};
	ActiveInfo {
		id: p.id.to_string(),
		name: p.name.clone(),
		kind: p.kind.clone(),
		fingerprint: p.fingerprint(),
		reachable_at,
		tailscale,
	}
}

// ---------------------------------------------------------------- settings

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> AppResult<SettingsDto> {
	let s = state.inner().0.settings.lock().unwrap();
	Ok(SettingsDto {
		rdp_client: s.rdp_client.clone(),
		terminal: s.terminal.clone(),
	})
}

#[tauri::command]
pub async fn set_settings(
	state: State<'_, AppState>,
	rdp_client: Option<String>,
	terminal: Option<String>,
) -> AppResult<()> {
	let state = state.inner().clone();
	{
		let mut s = state.0.settings.lock().unwrap();
		s.rdp_client = rdp_client.filter(|x| !x.trim().is_empty());
		s.terminal = terminal.filter(|x| !x.trim().is_empty());
	}
	state.save_settings()
}

// ---------------------------------------------------------------- registry

#[tauri::command]
pub async fn list_clients(state: State<'_, AppState>) -> AppResult<Vec<ClientDto>> {
	let Some(ctrl) = state.inner().active() else {
		return Ok(Vec::new());
	};
	let clients: Vec<Client> = ctrl.store.lock().unwrap().list().to_vec();
	let live = ctrl.live.lock().unwrap();
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
	let ctrl = state.require_active()?;
	let name = name.trim().to_string();
	if name.is_empty() {
		return Err(AppError::msg("name cannot be empty"));
	}
	let client = ctrl.store.lock().unwrap().create(name, os)?;
	let token = client.enrollment_token.clone().unwrap_or_default();
	let deploy_script = deploy::script(client.os, &token, &deploy_target(&ctrl).await);
	state.notify_changed();
	Ok(CreateClientResult {
		client: to_dto(&client, false, None),
		deploy_script,
	})
}

#[tauri::command]
pub async fn remove_client(state: State<'_, AppState>, id: String) -> AppResult<()> {
	let state = state.inner().clone();
	let ctrl = state.require_active()?;
	let id = parse_id(&id)?;
	session::stop(&ctrl, id);
	if let Some(link) = ctrl.connection(id) {
		link.close();
	}
	ctrl.live.lock().unwrap().remove(&id);
	ctrl.store.lock().unwrap().remove(id)?;
	state.notify_changed();
	Ok(())
}

#[tauri::command]
pub async fn rename_client(state: State<'_, AppState>, id: String, name: String) -> AppResult<()> {
	let state = state.inner().clone();
	let ctrl = state.require_active()?;
	let id = parse_id(&id)?;
	let name = name.trim().to_string();
	if name.is_empty() {
		return Err(AppError::msg("name cannot be empty"));
	}
	ctrl.store.lock().unwrap().rename(id, name)?;
	state.notify_changed();
	Ok(())
}

#[tauri::command]
pub async fn get_deploy_script(state: State<'_, AppState>, id: String, os: Option<ClientOs>) -> AppResult<String> {
	let ctrl = state.inner().require_active()?;
	let id = parse_id(&id)?;
	let (client_os, token) = {
		let store = ctrl.store.lock().unwrap();
		let c = store.get(id).ok_or(AppError::NotFound)?;
		(c.os, c.enrollment_token.clone())
	};
	let token = token.ok_or_else(|| AppError::msg("client already enrolled — reset its token to re-deploy"))?;
	Ok(deploy::script(
		os.unwrap_or(client_os),
		&token,
		&deploy_target(&ctrl).await,
	))
}

#[tauri::command]
pub async fn reset_token(state: State<'_, AppState>, id: String) -> AppResult<CreateClientResult> {
	let state = state.inner().clone();
	let ctrl = state.require_active()?;
	let id = parse_id(&id)?;
	let token = ctrl.store.lock().unwrap().reset_token(id)?;
	let target = deploy_target(&ctrl).await;
	let client = {
		let store = ctrl.store.lock().unwrap();
		store.get(id).ok_or(AppError::NotFound)?.clone()
	};
	let deploy_script = deploy::script(client.os, &token, &target);
	state.notify_changed();
	Ok(CreateClientResult {
		client: to_dto(&client, ctrl.is_online(id), None),
		deploy_script,
	})
}

// ---------------------------------------------------------------- live control

#[tauri::command]
pub async fn client_status(state: State<'_, AppState>, id: String) -> AppResult<AgentStatus> {
	let ctrl = state.inner().require_active()?;
	let id = parse_id(&id)?;
	let conn = ctrl.connection(id).ok_or(AppError::Offline)?;
	match control_request(&conn, &ControlRequest::Status).await? {
		ControlResponse::Status(status) => {
			if let Some(live) = ctrl.live.lock().unwrap().get_mut(&id) {
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
	let ctrl = state.inner().require_active()?;
	let id = parse_id(&id)?;
	let conn = ctrl.connection(id).ok_or(AppError::Offline)?;
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
	let ctrl = state.inner().require_active()?;
	let id = parse_id(&id)?;
	let conn = ctrl.connection(id).ok_or(AppError::Offline)?;
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
	let ctrl = state.require_active()?;
	let id = parse_id(&id)?;
	let defaults = SessionConfig::default();
	let cfg = SessionConfig {
		display: display.unwrap_or(defaults.display),
		quality: quality.unwrap_or(defaults.quality),
		max_fps: max_fps.unwrap_or(defaults.max_fps),
	};
	session::start(&state, ctrl, id, cfg);
	Ok(())
}

#[tauri::command]
pub async fn send_input(state: State<'_, AppState>, id: String, event: InputEvent) -> AppResult<()> {
	let ctrl = state.inner().require_active()?;
	session::send_input(&ctrl, parse_id(&id)?, event)
}

#[tauri::command]
pub async fn stop_control(state: State<'_, AppState>, id: String) -> AppResult<()> {
	let ctrl = state.inner().require_active()?;
	session::stop(&ctrl, parse_id(&id)?);
	Ok(())
}

/// Resolve `(host, port)` to point a client at — tunneling through the relay
/// when the agent isn't directly reachable.
async fn client_endpoint(conn: &AgentLink, reported: Option<String>, remote_port: u16) -> AppResult<(String, u16)> {
	if conn.is_relay() {
		let local = crate::tunnel::open(conn.clone(), remote_port).await?;
		return Ok(("127.0.0.1".to_string(), local));
	}
	let host = reported
		.filter(|s| !s.is_empty())
		.or_else(|| conn.remote_address().map(|a| a.ip().to_string()))
		.ok_or_else(|| AppError::msg("no reachable address for this client"))?;
	Ok((host, remote_port))
}

/// Enable RDP on the client and launch the host's RDP viewer.
#[tauri::command]
pub async fn connect_rdp(state: State<'_, AppState>, id: String) -> AppResult<()> {
	let state = state.inner().clone();
	let ctrl = state.require_active()?;
	let id = parse_id(&id)?;
	let conn = ctrl.connection(id).ok_or(AppError::Offline)?;
	let info = match control_request(&conn, &ControlRequest::EnableRdp).await? {
		ControlResponse::Rdp(info) => info,
		ControlResponse::Error { message } => return Err(AppError::Agent(message)),
		_ => return Err(AppError::msg("unexpected response")),
	};
	let (host, port) = client_endpoint(&conn, info.address.clone(), info.port).await?;
	let pref = state.0.settings.lock().unwrap().rdp_client.clone();
	crate::rdp::launch(pref.as_deref(), &host, port, &info.username, info.password.as_deref())
}

/// Probe that an SSH server actually answers at `host:port` (through the tunnel
/// in relay mode) before launching a terminal — otherwise `ssh` just flashes
/// open and closes with no explanation when the client has no SSH server.
async fn ssh_reachable(host: &str, port: u16) -> bool {
	use tokio::io::AsyncReadExt;
	let connect = tokio::net::TcpStream::connect((host, port));
	let Ok(Ok(mut stream)) = tokio::time::timeout(std::time::Duration::from_secs(6), connect).await else {
		return false;
	};
	let mut buf = [0u8; 1];
	matches!(
		tokio::time::timeout(std::time::Duration::from_secs(6), stream.read(&mut buf)).await,
		Ok(Ok(n)) if n > 0
	)
}

/// Open a terminal SSH session to a client.
#[tauri::command]
pub async fn connect_ssh(state: State<'_, AppState>, id: String) -> AppResult<()> {
	let state = state.inner().clone();
	let ctrl = state.require_active()?;
	let id = parse_id(&id)?;
	let conn = ctrl.connection(id).ok_or(AppError::Offline)?;
	let status = match control_request(&conn, &ControlRequest::Status).await? {
		ControlResponse::Status(s) => s,
		ControlResponse::Error { message } => return Err(AppError::Agent(message)),
		_ => return Err(AppError::msg("unexpected response")),
	};
	let (host, port) = client_endpoint(&conn, status.tailscale_ip.clone(), 22).await?;
	if !ssh_reachable(&host, port).await {
		return Err(AppError::msg(format!(
			"Couldn't reach an SSH server on {}. Make sure an SSH server (e.g. openssh-server) is installed and running on the client.",
			status.host.hostname
		)));
	}
	let terminal = state.0.settings.lock().unwrap().terminal.clone();
	crate::ssh::launch(terminal.as_deref(), &host, port, &status.host.username)
}

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
