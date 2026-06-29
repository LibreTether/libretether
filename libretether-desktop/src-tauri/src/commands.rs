//! Tauri commands — the surface the React UI calls into.

use libretether_protocol::{
	AgentStatus, ControlRequest, ControlResponse, ExecResult, InputEvent, ScreenshotResult, SessionConfig, DEFAULT_PORT,
};
use serde::Serialize;
use tauri::State;
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
	/// Preferred RDP client key or custom command.
	pub rdp_client: Option<String>,
	/// Preferred terminal launcher for SSH.
	pub terminal: Option<String>,
	/// Relay address (empty = listen mode / direct + Tailscale).
	pub relay_addr: Option<String>,
	pub relay_owner_secret: Option<String>,
	pub relay_agent_secret: Option<String>,
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

/// Build the deploy target from config: relay when a relay is configured, else
/// a direct/Tailscale controller connection.
async fn deploy_target(state: &AppState) -> deploy::DeployTarget {
	let (relay_addr, agent_secret) = {
		let cfg = state.0.config.lock().unwrap();
		(
			cfg.relay().map(|s| with_port(s, DEFAULT_PORT)),
			cfg.relay_agent_secret.clone().unwrap_or_default(),
		)
	};
	if let Some(address) = relay_addr {
		return deploy::DeployTarget::Relay { address, agent_secret };
	}
	deploy::DeployTarget::Controller {
		address: controller_addr(state).await,
		auth_key: auth_key(state),
	}
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
	let token = client.enrollment_token.clone().unwrap_or_default();
	let target = deploy_target(&state).await;
	let deploy_script = deploy::script(client.os, &token, &target);
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
	if let Some(link) = state.connection(id) {
		link.close();
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
	let (client_os, token) = {
		let store = state.0.store.lock().unwrap();
		let c = store.get(id).ok_or(AppError::NotFound)?;
		(c.os, c.enrollment_token.clone())
	};
	let token = token.ok_or_else(|| AppError::msg("client already enrolled — reset its token to re-deploy"))?;
	let target = deploy_target(&state).await;
	Ok(deploy::script(os.unwrap_or(client_os), &token, &target))
}

#[tauri::command]
pub async fn reset_token(state: State<'_, AppState>, id: String) -> AppResult<CreateClientResult> {
	let state = state.inner().clone();
	let id = parse_id(&id)?;
	let token = state.0.store.lock().unwrap().reset_token(id)?;
	// Resolve the deploy target with no lock held across the await.
	let target = deploy_target(&state).await;
	let client = {
		let store = state.0.store.lock().unwrap();
		store.get(id).ok_or(AppError::NotFound)?.clone()
	};
	let deploy_script = deploy::script(client.os, &token, &target);
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

/// Enable RDP on the client and launch the host's RDP viewer at its tailnet IP.
#[tauri::command]
pub async fn connect_rdp(state: State<'_, AppState>, id: String) -> AppResult<()> {
	let state = state.inner().clone();
	let id = parse_id(&id)?;
	let conn = state.connection(id).ok_or(AppError::Offline)?;
	let info = match control_request(&conn, &ControlRequest::EnableRdp).await? {
		ControlResponse::Rdp(info) => info,
		ControlResponse::Error { message } => return Err(AppError::Agent(message)),
		_ => return Err(AppError::msg("unexpected response")),
	};
	// In relay mode the client isn't directly reachable, so tunnel a loopback
	// port to its RDP server; otherwise dial its tailnet/reported address.
	let (host, port) = client_endpoint(&conn, info.address.clone(), info.port).await?;
	let pref = state.0.config.lock().unwrap().rdp_client.clone();
	crate::rdp::launch(pref.as_deref(), &host, port, &info.username, info.password.as_deref())
}

/// Resolve `(host, port)` to point a client at — tunneling through the relay
/// when the agent isn't directly reachable.
async fn client_endpoint(
	conn: &crate::link::AgentLink,
	reported: Option<String>,
	remote_port: u16,
) -> AppResult<(String, u16)> {
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

/// Probe that an SSH server actually answers at `host:port` (through the tunnel
/// in relay mode) before launching a terminal — otherwise `ssh` just flashes
/// open and closes with no explanation when the client has no SSH server.
async fn ssh_reachable(host: &str, port: u16) -> bool {
	use tokio::io::AsyncReadExt;
	let connect = tokio::net::TcpStream::connect((host, port));
	let Ok(Ok(mut stream)) = tokio::time::timeout(std::time::Duration::from_secs(6), connect).await else {
		return false;
	};
	// A real SSH server greets with an "SSH-..." banner as soon as you connect.
	let mut buf = [0u8; 1];
	matches!(
		tokio::time::timeout(std::time::Duration::from_secs(6), stream.read(&mut buf)).await,
		Ok(Ok(n)) if n > 0
	)
}

/// Open a terminal SSH session to a client at its tailnet IP.
#[tauri::command]
pub async fn connect_ssh(state: State<'_, AppState>, id: String) -> AppResult<()> {
	let state = state.inner().clone();
	let id = parse_id(&id)?;
	let conn = state.connection(id).ok_or(AppError::Offline)?;
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
	let terminal = state.0.config.lock().unwrap().terminal.clone();
	crate::ssh::launch(terminal.as_deref(), &host, port, &status.host.username)
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
	let c = {
		let cfg = state.0.config.lock().unwrap();
		ControllerConfigDto {
			listen_port: cfg.listen_port,
			fingerprint: cfg.fingerprint(),
			advertise_addr: cfg.advertise_addr.clone(),
			tailscale_auth_key: cfg.tailscale_auth_key.clone(),
			rdp_client: cfg.rdp_client.clone(),
			terminal: cfg.terminal.clone(),
			relay_addr: cfg.relay_addr.clone(),
			relay_owner_secret: cfg.relay_owner_secret.clone(),
			relay_agent_secret: cfg.relay_agent_secret.clone(),
		}
	};
	Ok(ControllerInfo {
		listen_port: c.listen_port,
		fingerprint: c.fingerprint,
		tailscale: tailscale::status().await,
		advertise_addr: c.advertise_addr,
		tailscale_auth_key: c.tailscale_auth_key,
		rdp_client: c.rdp_client,
		terminal: c.terminal,
		relay_addr: c.relay_addr,
		relay_owner_secret: c.relay_owner_secret,
		relay_agent_secret: c.relay_agent_secret,
	})
}

/// Plain snapshot of config fields, so the lock isn't held across the await.
struct ControllerConfigDto {
	listen_port: u16,
	fingerprint: String,
	advertise_addr: Option<String>,
	tailscale_auth_key: Option<String>,
	rdp_client: Option<String>,
	terminal: Option<String>,
	relay_addr: Option<String>,
	relay_owner_secret: Option<String>,
	relay_agent_secret: Option<String>,
}

/// Update the address agents dial and/or the embedded Tailscale auth key. Empty
/// strings clear the setting.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn set_controller_settings(
	state: State<'_, AppState>,
	advertise_addr: Option<String>,
	tailscale_auth_key: Option<String>,
	rdp_client: Option<String>,
	terminal: Option<String>,
	relay_addr: Option<String>,
	relay_owner_secret: Option<String>,
	relay_agent_secret: Option<String>,
) -> AppResult<()> {
	let state = state.inner().clone();
	let clean = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
	{
		let mut cfg = state.0.config.lock().unwrap();
		cfg.advertise_addr = clean(advertise_addr);
		cfg.tailscale_auth_key = clean(tailscale_auth_key);
		cfg.rdp_client = clean(rdp_client);
		cfg.terminal = clean(terminal);
		cfg.relay_addr = clean(relay_addr);
		cfg.relay_owner_secret = clean(relay_owner_secret);
		cfg.relay_agent_secret = clean(relay_agent_secret);
	}
	state.save_config()?;
	state.notify_changed();
	Ok(())
}
