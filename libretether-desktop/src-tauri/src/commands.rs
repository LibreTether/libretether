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

/// Send a control request to the active agent and extract the expected response
/// variant, mapping an agent-side `Error` to [`AppError::Agent`] and any other
/// variant to a generic error. Expands to an `AppResult<T>`.
macro_rules! expect_response {
	($conn:expr, $req:expr, $variant:path) => {
		match control_request($conn, $req).await? {
			$variant(value) => Ok(value),
			ControlResponse::Error { message } => Err(AppError::Agent(message)),
			_ => Err(AppError::msg("unexpected response from agent")),
		}
	};
}

// ---------------------------------------------------------------- DTOs

#[derive(Serialize)]
pub struct ClientDto {
	pub id: String,
	pub name: String,
	/// Serializes to the same "linux"/"macos"/"windows" literal the frontend reads,
	/// but carried as the enum so the conversion stays type-checked end to end.
	pub os: ClientOs,
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
		os: client.os,
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

/// Trim a user-entered name and reject a blank one. Shared by the create/rename
/// commands so the "name cannot be empty" rule lives in one place.
fn non_empty_name(name: String) -> AppResult<String> {
	let name = name.trim().to_string();
	if name.is_empty() {
		Err(AppError::msg("name cannot be empty"))
	} else {
		Ok(name)
	}
}

/// The active controller plus a parsed client id — the prelude almost every
/// per-client command shares (`require_active()? + parse_id()?`).
fn active_and_id(state: &AppState, id: &str) -> AppResult<(std::sync::Arc<ActiveController>, Uuid)> {
	Ok((state.require_active()?, parse_id(id)?))
}

fn with_port(addr: &str, port: u16) -> String {
	libretether_common::with_default_port(addr, port)
}

async fn tailscale_addr(port: u16) -> AppResult<String> {
	match tailscale::status().await.address {
		Some(addr) => Ok(format!("{addr}:{port}")),
		None => Err(AppError::msg(
			"Tailscale isn't up, so the controller's tailnet address is unknown — start Tailscale (or switch the controller to Direct/Relay) before deploying clients.",
		)),
	}
}

fn direct_addr(advertise: Option<String>, port: u16) -> AppResult<String> {
	match advertise.filter(|s| !s.trim().is_empty()) {
		Some(addr) => Ok(with_port(&addr, port)),
		None => Err(AppError::msg(
			"This controller has no advertise address — set one in the controller settings so clients know where to dial.",
		)),
	}
}

/// Build the deploy target for the active controller from its kind. Fails closed
/// (rather than emitting a `<controller-address>` placeholder) when the address
/// can't be determined, so a generated command is always actually runnable.
async fn deploy_target(ctrl: &ActiveController) -> AppResult<deploy::DeployTarget> {
	let controller_key = ctrl.profile.public_key();
	Ok(match ctrl.profile.kind.clone() {
		ControllerKind::Relay {
			address, agent_secret, ..
		} => deploy::DeployTarget::Relay {
			address: with_port(&address, DEFAULT_PORT),
			agent_secret,
			controller_key,
		},
		ControllerKind::Direct {
			advertise_addr,
			listen_port,
		} => deploy::DeployTarget::Controller {
			address: direct_addr(advertise_addr, listen_port)?,
			auth_key: None,
			controller_key,
		},
		ControllerKind::Tailscale { auth_key, listen_port } => deploy::DeployTarget::Controller {
			address: tailscale_addr(listen_port).await?,
			auth_key: auth_key.filter(|s| !s.trim().is_empty()),
			controller_key,
		},
	})
}

// ---------------------------------------------------------------- input safety

// Values an agent reports about itself (username, address, RDP password) are
// untrusted — a malicious or compromised client controls them, and we hand them
// to external programs (ssh, RDP viewers, the terminal). These validators reject
// anything outside a conservative safe set so a client can't inject shell /
// AppleScript / cmd commands into the controller operator's machine.

fn safe_username(s: &str) -> AppResult<String> {
	let s = s.trim();
	let ok = !s.is_empty()
		&& s.len() <= 64
		&& !s.starts_with('-')
		&& s.chars()
			.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '\\'));
	if ok {
		Ok(s.to_string())
	} else {
		Err(AppError::msg(format!("client reported an unsafe username ({s:?})")))
	}
}

fn safe_host(s: &str) -> AppResult<String> {
	let s = s.trim();
	if s.parse::<std::net::IpAddr>().is_ok() {
		return Ok(s.to_string());
	}
	let ok = !s.is_empty()
		&& s.len() <= 253
		&& !s.starts_with('-')
		&& s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
	if ok {
		Ok(s.to_string())
	} else {
		Err(AppError::msg(format!("client reported an unsafe address ({s:?})")))
	}
}

fn safe_password(s: &str) -> AppResult<String> {
	let ok = !s.is_empty()
		&& s.len() <= 128
		&& !s
			.chars()
			.any(|c| c.is_control() || c.is_whitespace() || "'\"@/\\:`$&|;<>(){}".contains(c));
	if ok {
		Ok(s.to_string())
	} else {
		Err(AppError::msg("client reported an RDP password with unsafe characters"))
	}
}

/// Validate an operator-entered launcher template (the `rdp_client` / `terminal`
/// settings). These are split on whitespace and run as `Command::new(first).args(rest)`
/// — never through a shell — so the risk isn't shell metacharacters but the *binary
/// position*: a compromised webview (the same threat model `save_text_file` defends
/// against) could set a template whose program is a `{host}`-style placeholder that
/// then expands to an agent-reported value and gets executed. Reject a placeholder
/// or a leading-`-` flag in the program position, plus control chars / absurd
/// length; the operator is otherwise trusted to name their own viewer/terminal.
fn safe_launch_template(s: &str) -> AppResult<String> {
	let s = s.trim();
	if s.len() > 512 {
		return Err(AppError::msg("the launcher command is too long"));
	}
	if s.chars().any(|c| c.is_control()) {
		return Err(AppError::msg("the launcher command contains control characters"));
	}
	let bin = s
		.split_whitespace()
		.next()
		.ok_or_else(|| AppError::msg("the launcher command is empty"))?;
	if bin.contains('{') || bin.contains('}') {
		return Err(AppError::msg(
			"the launcher command's program must be a literal command, not a {placeholder}",
		));
	}
	if bin.starts_with('-') {
		return Err(AppError::msg("the launcher command's program can't start with '-'"));
	}
	Ok(s.to_string())
}

/// Trim a launcher setting, treating blank as "unset" and validating a non-blank
/// value with [`safe_launch_template`].
fn normalize_template(value: Option<String>) -> AppResult<Option<String>> {
	match value.map(|x| x.trim().to_string()).filter(|x| !x.is_empty()) {
		Some(v) => Ok(Some(safe_launch_template(&v)?)),
		None => Ok(None),
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
	let profile = state.create_profile(non_empty_name(name)?, kind)?;
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
	let profile = state.update_profile(id, non_empty_name(name)?, kind)?;
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
	// Validate before touching state (and before taking the lock): these settings
	// are later exec'd, so a compromised webview must not be able to point them at
	// an arbitrary program via a placeholder in the binary position.
	let rdp_client = normalize_template(rdp_client)?;
	let terminal = normalize_template(terminal)?;
	let state = state.inner().clone();
	{
		let mut s = state.0.settings.lock().unwrap();
		s.rdp_client = rdp_client;
		s.terminal = terminal;
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
	let name = non_empty_name(name)?;
	// Resolve the deploy target first: if the controller can't say where clients
	// should dial (e.g. Direct with no advertise address), fail closed *before*
	// creating a client we couldn't generate a runnable command for.
	let target = deploy_target(&ctrl).await?;
	let client = ctrl.mutate_store(|s| Ok(s.create(name, os)))?;
	let token = client.enrollment_token.clone().unwrap_or_default();
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
	let (ctrl, id) = active_and_id(&state, &id)?;
	crate::logbook::info(
		"controller",
		&format!("removing client {id}: stopping session and tunnels"),
	);
	session::stop(&ctrl, id);
	crate::tunnel::close_for(&ctrl, id);
	if let Some(link) = ctrl.connection(id) {
		link.close();
	}
	ctrl.live.lock().unwrap().remove(&id);
	ctrl.mutate_store(|s| s.remove(id))?;
	state.notify_changed();
	Ok(())
}

#[tauri::command]
pub async fn rename_client(state: State<'_, AppState>, id: String, name: String) -> AppResult<()> {
	let state = state.inner().clone();
	let (ctrl, id) = active_and_id(&state, &id)?;
	let name = non_empty_name(name)?;
	ctrl.mutate_store(|s| s.rename(id, name))?;
	state.notify_changed();
	Ok(())
}

#[tauri::command]
pub async fn get_deploy_script(state: State<'_, AppState>, id: String, os: Option<ClientOs>) -> AppResult<String> {
	let (ctrl, id) = active_and_id(state.inner(), &id)?;
	let (client_os, token) = {
		let store = ctrl.store.lock().unwrap();
		let c = store.get(id).ok_or(AppError::NotFound)?;
		(c.os, c.enrollment_token.clone())
	};
	let token = token.ok_or(AppError::AlreadyEnrolled)?;
	let target = deploy_target(&ctrl).await?;
	Ok(deploy::script(os.unwrap_or(client_os), &token, &target))
}

#[tauri::command]
pub async fn reset_token(state: State<'_, AppState>, id: String) -> AppResult<CreateClientResult> {
	let state = state.inner().clone();
	let (ctrl, id) = active_and_id(&state, &id)?;
	let token = ctrl.mutate_store(|s| s.reset_token(id))?;
	let target = deploy_target(&ctrl).await?;
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
	let (ctrl, id) = active_and_id(state.inner(), &id)?;
	let conn = ctrl.connection(id).ok_or(AppError::Offline)?;
	let status = expect_response!(&conn, &ControlRequest::Status, ControlResponse::Status)?;
	if let Some(live) = ctrl.live.lock().unwrap().get_mut(&id) {
		live.status = Some(status.clone());
	}
	Ok(status)
}

#[tauri::command]
pub async fn client_exec(
	state: State<'_, AppState>,
	id: String,
	program: String,
	args: Vec<String>,
	timeout_secs: Option<u64>,
) -> AppResult<ExecResult> {
	let (ctrl, id) = active_and_id(state.inner(), &id)?;
	let conn = ctrl.connection(id).ok_or(AppError::Offline)?;
	// Audit the remote command (this is an arbitrary-command surface by design):
	// log the target and program + arg count, but not the argument values, which
	// can carry secrets. Mirrors the connection-event logging in `server.rs`.
	crate::logbook::info("controller", &format!("exec on {id}: {program} ({} args)", args.len()));
	expect_response!(
		&conn,
		&ControlRequest::Exec {
			program,
			args,
			timeout_secs,
		},
		ControlResponse::Exec
	)
}

#[tauri::command]
pub async fn client_screenshot(
	state: State<'_, AppState>,
	id: String,
	display: Option<u32>,
) -> AppResult<ScreenshotResult> {
	let (ctrl, id) = active_and_id(state.inner(), &id)?;
	let conn = ctrl.connection(id).ok_or(AppError::Offline)?;
	expect_response!(
		&conn,
		&ControlRequest::Screenshot { display },
		ControlResponse::Screenshot
	)
}

// ---------------------------------------------------------------- session

#[tauri::command]
pub async fn start_control(
	state: State<'_, AppState>,
	id: String,
	config: SessionConfig,
	frames: tauri::ipc::Channel,
) -> AppResult<()> {
	let state = state.inner().clone();
	let (ctrl, id) = active_and_id(&state, &id)?;
	session::start(&state, ctrl, id, config.sanitized(), frames);
	Ok(())
}

/// Change the live session's quality/fps/scale without restarting it.
#[tauri::command]
pub async fn configure_control(state: State<'_, AppState>, id: String, config: SessionConfig) -> AppResult<()> {
	let ctrl = state.inner().require_active()?;
	session::configure(&ctrl, parse_id(&id)?, config.sanitized())
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
async fn client_endpoint(
	ctrl: &ActiveController,
	id: Uuid,
	conn: &AgentLink,
	reported: Option<String>,
	remote_port: u16,
) -> AppResult<(String, u16)> {
	if conn.is_relay() {
		let local = crate::tunnel::open(ctrl, id, conn.clone(), remote_port).await?;
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
	let (ctrl, id) = active_and_id(&state, &id)?;
	let conn = ctrl.connection(id).ok_or(AppError::Offline)?;
	crate::logbook::info("rdp", &format!("connecting RDP to {id}: enabling RDP on the agent"));
	let info = expect_response!(&conn, &ControlRequest::EnableRdp, ControlResponse::Rdp)?;
	let (host, port) = client_endpoint(&ctrl, id, &conn, info.address.clone(), info.port).await?;
	let host = safe_host(&host)?;
	let username = safe_username(&info.username)?;
	let password = info.password.as_deref().map(safe_password).transpose()?;
	let pref = state.0.settings.lock().unwrap().rdp_client.clone();
	crate::logbook::info("rdp", &format!("launching RDP viewer for {host}:{port}"));
	crate::rdp::launch(pref.as_deref(), &host, port, &username, password.as_deref())
}

/// Open a terminal SSH session to a client. Prefers a real SSH server on the
/// client; if none is listening, falls back to the agent's built-in server so SSH
/// works even on a machine with no `sshd` installed (e.g. a stock Windows box).
#[tauri::command]
pub async fn connect_ssh(state: State<'_, AppState>, id: String) -> AppResult<()> {
	let state = state.inner().clone();
	let (ctrl, id) = active_and_id(&state, &id)?;
	let conn = ctrl.connection(id).ok_or(AppError::Offline)?;
	crate::logbook::info(
		"ssh",
		&format!("connecting SSH to {id}: probing for a system SSH server"),
	);
	let status = expect_response!(&conn, &ControlRequest::Status, ControlResponse::Status)?;
	let terminal = state.0.settings.lock().unwrap().terminal.clone();

	// Ask the agent whether a system SSH server is listening on its own loopback.
	// Having the agent probe is end-to-end in both direct and relay mode — a
	// controller-side TCP connect would, in relay mode, only reach the
	// always-accepting local tunnel listener and couldn't tell a live server apart.
	let probe = expect_response!(
		&conn,
		&ControlRequest::ProbePort { port: 22 },
		ControlResponse::PortReachable
	)?;
	if probe.reachable {
		// A real SSH server is present — reach it as before (direct IP, or a tunnel
		// to port 22 in relay mode) and let the client's own credentials apply.
		let (host, port) = client_endpoint(&ctrl, id, &conn, status.tailscale_ip.clone(), 22).await?;
		let host = safe_host(&host)?;
		let username = safe_username(&status.host.username)?;
		crate::logbook::info(
			"ssh",
			&format!("system SSH server found; launching terminal to {host}:{port}"),
		);
		return crate::ssh::launch(terminal.as_deref(), &host, port, &username, None);
	}

	// No system SSH server: start the agent's built-in one and connect to it with
	// the ephemeral key it returns. It binds loopback only, so we always tunnel to
	// it (even in direct mode, which normally dials the client's address directly).
	crate::logbook::info("ssh", "no system SSH server; starting the agent's embedded SSH server");
	let info = expect_response!(&conn, &ControlRequest::EnableSsh, ControlResponse::Ssh)?;
	let username = safe_username(&info.username)?;
	let key_path = write_ephemeral_key(id, &info.private_key)?;
	let local = crate::tunnel::open(&ctrl, id, conn.clone(), info.port).await?;
	crate::logbook::info(
		"ssh",
		&format!("launching terminal to embedded SSH server via 127.0.0.1:{local}"),
	);
	crate::ssh::launch(terminal.as_deref(), "127.0.0.1", local, &username, Some(&key_path))
}

/// Persist the embedded SSH server's ephemeral private key (owner-only) so the
/// launched `ssh` process can read it via `-i`. Keyed by client id and overwritten
/// each connect so it doesn't accumulate; the terminal outlives this call, so the
/// file must stay on disk afterwards.
fn write_ephemeral_key(id: Uuid, pem: &str) -> AppResult<std::path::PathBuf> {
	let path = std::env::temp_dir().join("libretether-ssh").join(format!("{id}.key"));
	libretether_protocol::secret::write_str(&path, pem).map_err(AppError::Io)?;
	Ok(path)
}

/// The controller's own recent log lines (oldest first), to seed the Logs page.
/// Live updates arrive via the `logs:entry` event.
#[tauri::command]
pub async fn get_controller_logs() -> Vec<crate::logbook::LogEntry> {
	crate::logbook::entries(None)
}

/// Fetch a connected client's recent agent-log lines, normalised into the same
/// [`crate::logbook::LogEntry`] shape as the controller's own logs (tagged with
/// the client's name as the source) so the Logs page can show both together.
#[tauri::command]
pub async fn client_logs(
	state: State<'_, AppState>,
	id: String,
	max_lines: Option<u32>,
) -> AppResult<Vec<crate::logbook::LogEntry>> {
	let (ctrl, uuid) = active_and_id(state.inner(), &id)?;
	let conn = ctrl.connection(uuid).ok_or(AppError::Offline)?;
	let source = ctrl
		.store
		.lock()
		.unwrap()
		.get(uuid)
		.map(|c| c.name.clone())
		.unwrap_or_else(|| "agent".to_string());
	let result = expect_response!(&conn, &ControlRequest::FetchLogs { max_lines }, ControlResponse::Logs)?;
	// Re-anchor the agent's timestamps to OUR clock. The agent stamps lines with its
	// own wall clock, which may be a different timezone or simply skewed (common on
	// guest VMs), so trusting the absolute value makes its lines render at the wrong
	// local time. Shifting every line by (our_now - their_now) keeps each line's age
	// intact but expresses it in the controller's frame, matching controller/RDP logs.
	let offset = libretether_common::now_secs() as i64 - result.now_secs as i64;
	Ok(result
		.lines
		.into_iter()
		.map(|l| crate::logbook::LogEntry {
			ts_secs: (l.ts_secs as i64 + offset).max(0) as u64,
			level: l.level,
			source: source.clone(),
			message: l.message,
		})
		.collect())
}

#[tauri::command]
pub async fn save_text_file(path: String, contents: String) -> AppResult<()> {
	// The path comes from the user's own save dialog, but constrain it anyway so
	// a compromised webview can't write to arbitrary locations: require an
	// absolute path with an extension this app actually emits.
	let p = std::path::Path::new(&path);
	if !p.is_absolute() {
		return Err(AppError::msg("refusing to write a non-absolute path"));
	}
	// Reject `..` traversal so a path can't be steered up out of where the dialog
	// pointed (e.g. `/home/user/Downloads/../.config/autostart/x.sh`).
	if p.components().any(|c| c == std::path::Component::ParentDir) {
		return Err(AppError::msg("refusing to write a path containing '..'"));
	}
	if !matches!(p.extension().and_then(|e| e.to_str()), Some("sh" | "ps1" | "txt")) {
		return Err(AppError::msg(
			"refusing to write this file type (expected .sh, .ps1 or .txt)",
		));
	}
	// Don't follow a pre-existing symlink at the destination — that could redirect
	// the write to a target outside the chosen location.
	if let Ok(meta) = std::fs::symlink_metadata(p) {
		if meta.file_type().is_symlink() {
			return Err(AppError::msg("refusing to overwrite a symlink"));
		}
	}
	std::fs::write(&path, contents)?;
	#[cfg(unix)]
	if path.ends_with(".sh") {
		use std::os::unix::fs::PermissionsExt;
		let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	// These validators are the anti-injection guard for untrusted agent-reported
	// values (username/address/password) that get handed to ssh / RDP viewers /
	// the terminal. They must accept ordinary values and reject anything that
	// could break out into a shell/AppleScript/cmd.

	#[test]
	fn safe_username_accepts_reasonable_names() {
		for ok in ["alice", "user.name_1", "DOMAIN\\user", "a-b", "Administrator"] {
			assert!(safe_username(ok).is_ok(), "should accept {ok:?}");
		}
	}

	#[test]
	fn safe_username_rejects_injection_and_garbage() {
		let too_long = "x".repeat(65);
		let bad = [
			"",
			"   ",
			"-flag",
			"a b",
			"a;b",
			"a$b",
			"a`b",
			"user@host",
			"a/b",
			"a|b",
			"a&b",
			"a\nb",
			too_long.as_str(),
		];
		for b in bad {
			assert!(safe_username(b).is_err(), "should reject {b:?}");
		}
	}

	#[test]
	fn safe_host_accepts_ips_and_hostnames() {
		for ok in ["10.0.0.1", "::1", "fd00::1", "host.example", "a-b.example.com"] {
			assert!(safe_host(ok).is_ok(), "should accept {ok:?}");
		}
	}

	#[test]
	fn safe_host_rejects_injection_and_garbage() {
		let too_long = "x".repeat(254);
		let bad = [
			"",
			"  ",
			"-h",
			"a b",
			"host;rm -rf",
			"h$(x)",
			"h|y",
			"a/../b",
			"h&y",
			"h`y",
			too_long.as_str(),
		];
		for b in bad {
			assert!(safe_host(b).is_err(), "should reject {b:?}");
		}
	}

	#[test]
	fn safe_password_accepts_plain_alnum() {
		assert!(safe_password("Abc123xyzPlain").is_ok());
	}

	#[test]
	fn safe_password_rejects_metacharacters_and_whitespace() {
		let too_long = "x".repeat(129);
		let bad = [
			"",
			"has space",
			"a'b",
			"a\"b",
			"a@b",
			"a:b",
			"a/b",
			"a\\b",
			"a`b",
			"a$b",
			"a;b",
			"a|b",
			"a&b",
			"a<b",
			"a>b",
			"a(b",
			"a)b",
			"a{b",
			"a\nb",
			"a\tb",
			too_long.as_str(),
		];
		for b in bad {
			assert!(safe_password(b).is_err(), "should reject {b:?}");
		}
	}

	#[test]
	fn safe_launch_template_accepts_real_commands_with_placeholder_args() {
		// A literal program followed by flags and placeholders in the argument
		// positions is exactly what an operator legitimately configures.
		assert!(safe_launch_template("gnome-terminal --").is_ok());
		assert!(safe_launch_template("xterm -e").is_ok());
		assert!(safe_launch_template("/usr/bin/xfreerdp /v:{host}:{port} /u:{user}").is_ok());
		assert_eq!(safe_launch_template("  remmina -c  ").unwrap(), "remmina -c");
	}

	#[test]
	fn safe_launch_template_rejects_placeholder_or_flag_in_the_binary_position() {
		// A placeholder as the program would let an agent-reported value become the
		// executed binary — the core of the finding.
		assert!(safe_launch_template("{host} /v:1.2.3.4").is_err());
		assert!(safe_launch_template("{password}").is_err());
		// A leading-'-' program could be parsed as a flag; control chars / blank / huge.
		assert!(safe_launch_template("-evil").is_err());
		assert!(safe_launch_template("").is_err());
		assert!(safe_launch_template("foo\nbar").is_err());
		assert!(safe_launch_template(&"x".repeat(513)).is_err());
	}

	#[test]
	fn normalize_template_blanks_to_none_and_validates_non_blank() {
		assert_eq!(normalize_template(None).unwrap(), None);
		assert_eq!(normalize_template(Some("   ".into())).unwrap(), None);
		assert_eq!(
			normalize_template(Some("remmina".into())).unwrap(),
			Some("remmina".into())
		);
		assert!(normalize_template(Some("{host}".into())).is_err());
	}

	#[test]
	fn with_port_appends_only_when_missing() {
		assert_eq!(with_port("1.2.3.4", 47600), "1.2.3.4:47600");
		assert_eq!(with_port("host.example", 9000), "host.example:9000");
		assert_eq!(with_port("host:22", 47600), "host:22");
	}

	/// A fresh active controller with the given kind (no live agents).
	fn active_with(kind: ControllerKind) -> ActiveController {
		use crate::registry::ClientStore;
		use crate::state::ControllerProfile;
		use std::sync::atomic::{AtomicU32, Ordering};
		static N: AtomicU32 = AtomicU32::new(0);
		let path = std::env::temp_dir().join(format!(
			"lt-cmds-{}-{}.json",
			std::process::id(),
			N.fetch_add(1, Ordering::Relaxed)
		));
		ActiveController::new(
			ControllerProfile::new("test".into(), kind),
			ClientStore::load(path).unwrap(),
		)
	}

	#[tokio::test]
	async fn deploy_target_for_relay_carries_the_agent_secret_and_controller_key() {
		let ctrl = active_with(ControllerKind::Relay {
			address: "relay.example".into(),
			owner_secret: "owner".into(),
			agent_secret: "agentsecret".into(),
		});
		match deploy_target(&ctrl).await.unwrap() {
			deploy::DeployTarget::Relay {
				address,
				agent_secret,
				controller_key,
			} => {
				assert_eq!(address, "relay.example:47600", "the default port is appended");
				assert_eq!(agent_secret, "agentsecret");
				// The pinned controller key is the profile's real public key (not blank).
				assert_eq!(controller_key, ctrl.profile.public_key());
				assert!(!controller_key.is_empty());
			}
			_ => panic!("relay controller must produce a Relay deploy target"),
		}
	}

	#[tokio::test]
	async fn deploy_target_for_direct_uses_the_advertise_addr_and_no_auth_key() {
		let ctrl = active_with(ControllerKind::Direct {
			advertise_addr: Some("10.0.0.5".into()),
			listen_port: 47600,
		});
		match deploy_target(&ctrl).await.unwrap() {
			deploy::DeployTarget::Controller {
				address,
				auth_key,
				controller_key,
			} => {
				assert_eq!(address, "10.0.0.5:47600");
				assert!(auth_key.is_none(), "direct mode carries no tailscale key");
				assert_eq!(controller_key, ctrl.profile.public_key());
			}
			_ => panic!("direct controller must produce a Controller deploy target"),
		}
	}

	#[tokio::test]
	async fn deploy_target_for_direct_without_an_advertise_addr_fails_closed() {
		// No advertise address means there's no concrete host to put in the deploy
		// command — fail closed with a clear message rather than emit a placeholder
		// that produces a broken installer invocation.
		let ctrl = active_with(ControllerKind::Direct {
			advertise_addr: None,
			listen_port: 47600,
		});
		match deploy_target(&ctrl).await {
			Err(e) => assert!(e.to_string().contains("advertise address"), "unexpected error: {e}"),
			Ok(_) => panic!("a direct controller with no advertise address must fail closed"),
		}
	}

	#[test]
	fn direct_addr_appends_the_port_or_fails_closed() {
		assert_eq!(direct_addr(Some("10.0.0.5".into()), 47600).unwrap(), "10.0.0.5:47600");
		assert_eq!(direct_addr(Some("ctl:22".into()), 47600).unwrap(), "ctl:22");
		// Missing / blank advertise address fails closed (no placeholder).
		assert!(direct_addr(None, 47600).is_err());
		assert!(direct_addr(Some("   ".into()), 47600).is_err());
	}

	#[test]
	fn non_empty_name_trims_and_rejects_blank() {
		assert_eq!(non_empty_name("  box  ".into()).unwrap(), "box");
		assert!(non_empty_name("".into()).is_err());
		assert!(non_empty_name("   ".into()).is_err());
	}

	#[tokio::test]
	async fn save_text_file_rejects_unsafe_paths() {
		// Non-absolute, `..` traversal, and disallowed extensions are all refused
		// before any write happens.
		assert!(save_text_file("relative.sh".into(), "x".into()).await.is_err());
		assert!(
			save_text_file("/home/user/Downloads/../.config/autostart/x.sh".into(), "x".into())
				.await
				.is_err()
		);
		assert!(save_text_file("/tmp/evil.bashrc".into(), "x".into()).await.is_err());
		assert!(save_text_file("/tmp/payload.exe".into(), "x".into()).await.is_err());
	}

	// ------------------------------------------------ frontend wire contract
	//
	// `libretether-desktop/src/lib/types.ts` is hand-maintained to mirror these
	// structs. There is no codegen, so these tests are the drift guard: they pin the
	// exact JSON field set the frontend reads (and the enum tags it switches on). If
	// you rename/add/remove a field here, this fails until `types.ts` is updated to
	// match — turning a silent runtime `undefined` into a failed `cargo test`.

	use libretether_protocol::{HostInfo, MouseButton, SessionConfig};
	use serde::Serialize;

	/// Assert that `value` serializes to a JSON object with exactly `expected` keys.
	fn assert_fields<T: Serialize>(value: T, expected: &[&str]) {
		let v = serde_json::to_value(&value).unwrap();
		let obj = v.as_object().expect("expected a JSON object");
		let mut got: Vec<&str> = obj.keys().map(String::as_str).collect();
		got.sort_unstable();
		let mut want = expected.to_vec();
		want.sort_unstable();
		assert_eq!(got, want, "JSON field set drifted from src/lib/types.ts");
	}

	fn sample_host() -> HostInfo {
		HostInfo {
			hostname: "h".into(),
			os: "linux".into(),
			arch: "x86_64".into(),
			username: "u".into(),
		}
	}

	fn sample_status() -> AgentStatus {
		AgentStatus {
			host: sample_host(),
			agent_version: "1".into(),
			uptime_secs: 1,
			started_at: 0,
			boot_time_secs: None,
			displays: 1,
			tailscale_ip: None,
		}
	}

	#[test]
	fn protocol_dtos_match_types_ts() {
		assert_fields(sample_host(), &["hostname", "os", "arch", "username"]);
		assert_fields(
			sample_status(),
			&[
				"host",
				"agent_version",
				"uptime_secs",
				"started_at",
				"boot_time_secs",
				"displays",
				"tailscale_ip",
			],
		);
		assert_fields(
			ExecResult {
				code: Some(0),
				stdout: String::new(),
				stderr: String::new(),
				duration_ms: 0,
			},
			&["code", "stdout", "stderr", "duration_ms"],
		);
		assert_fields(
			ScreenshotResult {
				display: 0,
				width: 1,
				height: 1,
				png_base64: String::new(),
			},
			&["display", "width", "height", "png_base64"],
		);
		// The controller sends this to `start_control`/`configure_control`; the
		// frontend's SessionConfig mirror must carry exactly these keys.
		assert_fields(
			SessionConfig {
				display: 0,
				quality: 70,
				max_fps: 30,
				scale: 100,
				auto: false,
			},
			&["display", "quality", "max_fps", "scale", "auto"],
		);
	}

	#[test]
	fn command_dtos_match_types_ts() {
		let client = ClientDto {
			id: "id".into(),
			name: "n".into(),
			os: ClientOs::Linux,
			created_at: 0,
			enrolled: true,
			online: false,
			last_seen: None,
			status: None,
		};
		assert_fields(
			&client,
			&[
				"id",
				"name",
				"os",
				"created_at",
				"enrolled",
				"online",
				"last_seen",
				"status",
			],
		);
		assert_fields(
			CreateClientResult {
				client,
				deploy_script: String::new(),
			},
			&["client", "deploy_script"],
		);
		let kind = ControllerKind::Direct {
			advertise_addr: None,
			listen_port: 47600,
		};
		assert_fields(
			ControllerSummary {
				id: "id".into(),
				name: "n".into(),
				kind: kind.clone(),
				fingerprint: "fp".into(),
				machine_count: 0,
				active: false,
			},
			&["id", "name", "kind", "fingerprint", "machine_count", "active"],
		);
		assert_fields(
			ActiveInfo {
				id: "id".into(),
				name: "n".into(),
				kind,
				fingerprint: "fp".into(),
				reachable_at: None,
				tailscale: None,
			},
			&["id", "name", "kind", "fingerprint", "reachable_at", "tailscale"],
		);
		assert_fields(
			SettingsDto {
				rdp_client: None,
				terminal: None,
			},
			&["rdp_client", "terminal"],
		);
		assert_fields(
			TailscaleInfo {
				installed: false,
				running: false,
				address: None,
				hostname: None,
			},
			&["installed", "running", "address", "hostname"],
		);
		assert_fields(
			crate::logbook::LogEntry {
				ts_secs: 0,
				level: libretether_protocol::LogLevel::Info,
				source: "controller".into(),
				message: "hi".into(),
			},
			&["ts_secs", "level", "source", "message"],
		);
	}

	#[test]
	fn controller_kind_is_tagged_by_type_with_the_expected_fields() {
		let direct = serde_json::to_value(ControllerKind::Direct {
			advertise_addr: None,
			listen_port: 47600,
		})
		.unwrap();
		assert_eq!(direct["type"], "direct");
		assert_fields(
			ControllerKind::Direct {
				advertise_addr: None,
				listen_port: 47600,
			},
			&["type", "advertise_addr", "listen_port"],
		);
		assert_fields(
			ControllerKind::Tailscale {
				auth_key: None,
				listen_port: 47600,
			},
			&["type", "auth_key", "listen_port"],
		);
		assert_fields(
			ControllerKind::Relay {
				address: "a".into(),
				owner_secret: "o".into(),
				agent_secret: "g".into(),
			},
			&["type", "address", "owner_secret", "agent_secret"],
		);
		assert_eq!(
			serde_json::to_value(ControllerKind::Tailscale {
				auth_key: None,
				listen_port: 0
			})
			.unwrap()["type"],
			"tailscale"
		);
		assert_eq!(
			serde_json::to_value(ControllerKind::Relay {
				address: "a".into(),
				owner_secret: "o".into(),
				agent_secret: "g".into()
			})
			.unwrap()["type"],
			"relay"
		);
	}

	#[test]
	fn input_event_is_tagged_by_t_with_the_expected_variants() {
		let cases = [
			(
				InputEvent::MouseMove { x: 0.0, y: 0.0 },
				"mouse_move",
				&["t", "x", "y"][..],
			),
			(
				InputEvent::MouseButton {
					button: MouseButton::Left,
					pressed: true,
				},
				"mouse_button",
				&["t", "button", "pressed"][..],
			),
			(
				InputEvent::MouseScroll { dx: 0, dy: 0 },
				"mouse_scroll",
				&["t", "dx", "dy"][..],
			),
			(
				InputEvent::Key {
					code: "KeyA".into(),
					pressed: true,
				},
				"key",
				&["t", "code", "pressed"][..],
			),
			(InputEvent::Text { text: "x".into() }, "text", &["t", "text"][..]),
		];
		for (event, tag, fields) in cases {
			assert_eq!(serde_json::to_value(&event).unwrap()["t"], tag);
			assert_fields(event, fields);
		}
	}

	#[test]
	fn string_enums_serialize_to_the_literals_types_ts_expects() {
		assert_eq!(serde_json::to_value(MouseButton::Left).unwrap(), "left");
		assert_eq!(serde_json::to_value(MouseButton::Right).unwrap(), "right");
		assert_eq!(serde_json::to_value(MouseButton::Middle).unwrap(), "middle");
		assert_eq!(serde_json::to_value(ClientOs::Linux).unwrap(), "linux");
		assert_eq!(serde_json::to_value(ClientOs::Macos).unwrap(), "macos");
		assert_eq!(serde_json::to_value(ClientOs::Windows).unwrap(), "windows");
	}
}
