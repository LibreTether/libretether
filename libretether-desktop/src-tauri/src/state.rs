//! Controller profiles (multiple, persisted), global host settings, and the
//! single controller that is currently active (its identity, client registry,
//! live agents and sessions).
//!
//! Only one controller runs at a time. Selecting one loads its profile + store
//! and spawns its serve task; exiting aborts that task and clears live state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use libretether_protocol::crypto::Identity;
use libretether_protocol::{tls, AgentStatus, SessionClient, DEFAULT_PORT};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::link::AgentLink;
use crate::registry::{now_secs, ClientStore};

/// Event emitted whenever the client list or a client's connection state changes.
pub const EVENT_CHANGED: &str = "clients:changed";

/// How a controller reaches its agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControllerKind {
	/// Agents dial this machine directly (LAN, an existing VPN, or a port-forward).
	Direct {
		#[serde(default)]
		advertise_addr: Option<String>,
		listen_port: u16,
	},
	/// Agents join a tailnet with a pre-auth key, then dial this machine's tailnet address.
	Tailscale {
		#[serde(default)]
		auth_key: Option<String>,
		listen_port: u16,
	},
	/// The controller and all agents dial out to a `libretether-relay`.
	Relay {
		address: String,
		owner_secret: String,
		agent_secret: String,
	},
}

impl ControllerKind {
	/// The UDP port the controller listens on (relay mode dials out, so N/A).
	pub fn listen_port(&self) -> u16 {
		match self {
			ControllerKind::Direct { listen_port, .. } | ControllerKind::Tailscale { listen_port, .. } => *listen_port,
			ControllerKind::Relay { .. } => DEFAULT_PORT,
		}
	}

	/// The relay address when this is a relay controller.
	pub fn relay(&self) -> Option<&str> {
		match self {
			ControllerKind::Relay { address, .. } => Some(address.as_str()),
			_ => None,
		}
	}

	/// Reject incomplete settings (Tailscale needs an auth key; Relay needs an
	/// address + both secrets).
	pub fn validate(&self) -> AppResult<()> {
		let blank = |s: &Option<String>| s.as_deref().unwrap_or("").trim().is_empty();
		match self {
			ControllerKind::Tailscale { auth_key, .. } if blank(auth_key) => {
				Err(AppError::msg("Tailscale controllers require an auth key"))
			}
			ControllerKind::Relay {
				address,
				owner_secret,
				agent_secret,
			} if address.trim().is_empty() || owner_secret.trim().is_empty() || agent_secret.trim().is_empty() => Err(
				AppError::msg("Relay controllers require an address, owner secret and agent secret"),
			),
			_ => Ok(()),
		}
	}
}

/// A saved controller: its identity, QUIC certificate and connection settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerProfile {
	pub id: Uuid,
	pub name: String,
	pub created_at: u64,
	pub kind: ControllerKind,
	/// Base64 Ed25519 seed — the controller's own identity (shown as a fingerprint).
	pub identity_seed: String,
	/// Base64 DER self-signed certificate + PKCS8 key for the QUIC server.
	pub cert_der: String,
	pub key_der: String,
}

impl ControllerProfile {
	/// Create a profile with a fresh identity + certificate.
	pub fn new(name: String, kind: ControllerKind) -> Self {
		let (cert_der, key_der) = tls::self_signed();
		Self {
			id: Uuid::new_v4(),
			name,
			created_at: now_secs(),
			kind,
			identity_seed: Identity::generate().seed_b64(),
			cert_der: B64.encode(cert_der),
			key_der: B64.encode(key_der),
		}
	}

	pub fn cert_key_der(&self) -> AppResult<(Vec<u8>, Vec<u8>)> {
		let cert = B64
			.decode(&self.cert_der)
			.map_err(|e| AppError::msg(format!("bad cert: {e}")))?;
		let key = B64
			.decode(&self.key_der)
			.map_err(|e| AppError::msg(format!("bad key: {e}")))?;
		Ok((cert, key))
	}

	/// A short, human-comparable fingerprint of the controller identity.
	pub fn fingerprint(&self) -> String {
		Identity::from_seed_b64(&self.identity_seed)
			.map(|id| id.public_b64().chars().take(12).collect())
			.unwrap_or_else(|| "unknown".to_string())
	}

	pub fn public_key(&self) -> String {
		Identity::from_seed_b64(&self.identity_seed)
			.map(|id| id.public_b64())
			.unwrap_or_default()
	}

	/// The controller's signing identity, used to authenticate the controller to
	/// agents during the handshake.
	pub fn identity(&self) -> AppResult<Identity> {
		Identity::from_seed_b64(&self.identity_seed)
			.ok_or_else(|| AppError::msg("controller has an invalid identity seed"))
	}
}

/// Global host preferences, independent of any controller.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
	/// Preferred RDP client: "auto" | "freerdp" | "remmina" | "gnome-connections",
	/// or a custom command template (with {host} {port} {user} {password}).
	#[serde(default)]
	pub rdp_client: Option<String>,
	/// Preferred terminal launcher for SSH, e.g. "gnome-terminal --". Empty = auto-detect.
	#[serde(default)]
	pub terminal: Option<String>,
	/// The controller last connected to (so the UI can highlight it).
	#[serde(default)]
	pub last_controller: Option<Uuid>,
}

/// A live agent connection.
pub struct LiveConn {
	pub link: AgentLink,
	pub status: Option<AgentStatus>,
	/// Per-registration generation, so a stale connection's teardown can't evict
	/// a newer one that reconnected under the same client id (see `server::cleanup`).
	pub generation: u64,
}

/// A running screen-control session.
pub struct SessionHandle {
	pub input_tx: tokio::sync::mpsc::UnboundedSender<SessionClient>,
	pub task: tauri::async_runtime::JoinHandle<()>,
	pub token: u64,
}

/// A loopback tunnel forwarding to a client's RDP/SSH port through the relay.
/// Kept so it can be reused (instead of leaking a new listener per connect) and
/// torn down when the client is removed or the controller exits.
pub struct TunnelHandle {
	pub local_port: u16,
	pub task: tauri::async_runtime::JoinHandle<()>,
}

/// The single controller that is currently running.
pub struct ActiveController {
	pub profile: ControllerProfile,
	pub store: Mutex<ClientStore>,
	pub live: Mutex<HashMap<Uuid, LiveConn>>,
	pub sessions: Mutex<HashMap<Uuid, SessionHandle>>,
	/// Active loopback tunnels keyed by `(client, remote_port)`.
	pub tunnels: Mutex<HashMap<(Uuid, u16), TunnelHandle>>,
}

impl ActiveController {
	/// Clone out the link for a client, if connected.
	pub fn connection(&self, id: Uuid) -> Option<AgentLink> {
		self.live.lock().unwrap().get(&id).map(|c| c.link.clone())
	}

	pub fn is_online(&self, id: Uuid) -> bool {
		self.live.lock().unwrap().contains_key(&id)
	}
}

struct ActiveHandle {
	controller: Arc<ActiveController>,
	serve_task: tauri::async_runtime::JoinHandle<()>,
}

pub struct Inner {
	pub config_dir: PathBuf,
	pub settings: Mutex<Settings>,
	active: Mutex<Option<ActiveHandle>>,
	pub app: OnceLock<AppHandle>,
}

#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

impl AppState {
	pub fn init(config_dir: PathBuf) -> AppResult<Self> {
		std::fs::create_dir_all(config_dir.join("controllers"))?;
		let settings = load_settings(&config_dir)?;
		Ok(Self(Arc::new(Inner {
			config_dir,
			settings: Mutex::new(settings),
			active: Mutex::new(None),
			app: OnceLock::new(),
		})))
	}

	pub fn set_app(&self, app: AppHandle) {
		let _ = self.0.app.set(app);
	}

	/// Tell the frontend something changed so it can refresh.
	pub fn notify_changed(&self) {
		if let Some(app) = self.0.app.get() {
			let _ = app.emit(EVENT_CHANGED, ());
		}
	}

	// ---------------------------------------------------------------- profiles

	fn controllers_dir(&self) -> PathBuf {
		self.0.config_dir.join("controllers")
	}

	fn profile_dir(&self, id: Uuid) -> PathBuf {
		self.controllers_dir().join(id.to_string())
	}

	pub fn list_profiles(&self) -> AppResult<Vec<ControllerProfile>> {
		let mut out = Vec::new();
		if let Ok(entries) = std::fs::read_dir(self.controllers_dir()) {
			for entry in entries.flatten() {
				let path = entry.path().join("controller.json");
				if let Ok(raw) = std::fs::read_to_string(&path) {
					if let Ok(profile) = serde_json::from_str::<ControllerProfile>(&raw) {
						out.push(profile);
					}
				}
			}
		}
		out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
		Ok(out)
	}

	pub fn load_profile(&self, id: Uuid) -> AppResult<ControllerProfile> {
		let raw = std::fs::read_to_string(self.profile_dir(id).join("controller.json"))
			.map_err(|_| AppError::msg("controller not found"))?;
		serde_json::from_str(&raw).map_err(|e| AppError::msg(format!("parsing controller: {e}")))
	}

	fn save_profile(&self, profile: &ControllerProfile) -> AppResult<()> {
		let dir = self.profile_dir(profile.id);
		let raw =
			serde_json::to_string_pretty(profile).map_err(|e| AppError::msg(format!("serializing controller: {e}")))?;
		// Holds the owner/agent secrets, Tailscale auth key, identity seed and TLS
		// private key — write it owner-only.
		libretether_protocol::secret::write_str(dir.join("controller.json"), &raw)?;
		Ok(())
	}

	pub fn create_profile(&self, name: String, kind: ControllerKind) -> AppResult<ControllerProfile> {
		kind.validate()?;
		let profile = ControllerProfile::new(name, kind);
		self.save_profile(&profile)?;
		Ok(profile)
	}

	/// Update a profile's name + kind, preserving its identity, certificate and id.
	pub fn update_profile(&self, id: Uuid, name: String, kind: ControllerKind) -> AppResult<ControllerProfile> {
		if self.active().is_some_and(|c| c.profile.id == id) {
			return Err(AppError::msg("exit this controller before editing it"));
		}
		kind.validate()?;
		let mut profile = self.load_profile(id)?;
		profile.name = name;
		profile.kind = kind;
		self.save_profile(&profile)?;
		Ok(profile)
	}

	pub fn delete_profile(&self, id: Uuid) -> AppResult<()> {
		if self.active().is_some_and(|c| c.profile.id == id) {
			return Err(AppError::msg("exit this controller before deleting it"));
		}
		std::fs::remove_dir_all(self.profile_dir(id))?;
		Ok(())
	}

	/// Count enrolled machines for a (possibly inactive) controller, off disk.
	pub fn machine_count(&self, id: Uuid) -> usize {
		ClientStore::load(self.profile_dir(id).join("clients.json"))
			.map(|s| s.list().len())
			.unwrap_or(0)
	}

	// ---------------------------------------------------------------- settings

	pub fn save_settings(&self) -> AppResult<()> {
		let raw = {
			let settings = self.0.settings.lock().unwrap();
			serde_json::to_string_pretty(&*settings).map_err(|e| AppError::msg(format!("serializing settings: {e}")))?
		};
		std::fs::write(self.0.config_dir.join("settings.json"), raw)?;
		Ok(())
	}

	// ---------------------------------------------------------------- lifecycle

	pub fn active(&self) -> Option<Arc<ActiveController>> {
		self.0.active.lock().unwrap().as_ref().map(|h| h.controller.clone())
	}

	pub fn require_active(&self) -> AppResult<Arc<ActiveController>> {
		self.active().ok_or_else(|| AppError::msg("no controller is connected"))
	}

	/// Start serving a controller (replacing any currently-active one).
	pub fn activate(&self, id: Uuid) -> AppResult<Arc<ActiveController>> {
		self.deactivate();
		let profile = self.load_profile(id)?;
		let store = ClientStore::load(self.profile_dir(id).join("clients.json"))?;
		let controller = Arc::new(ActiveController {
			profile,
			store: Mutex::new(store),
			live: Mutex::new(HashMap::new()),
			sessions: Mutex::new(HashMap::new()),
			tunnels: Mutex::new(HashMap::new()),
		});

		let state = self.clone();
		let ctrl = controller.clone();
		let serve_task = tauri::async_runtime::spawn(async move {
			if ctrl.profile.kind.relay().is_some() {
				crate::server::serve_relay(state, ctrl).await;
			} else {
				crate::server::serve(state, ctrl).await;
			}
		});

		*self.0.active.lock().unwrap() = Some(ActiveHandle {
			controller: controller.clone(),
			serve_task,
		});
		{
			let mut settings = self.0.settings.lock().unwrap();
			settings.last_controller = Some(id);
		}
		let _ = self.save_settings();
		self.notify_changed();
		Ok(controller)
	}

	/// Stop the active controller (if any): abort its serve task + sessions and
	/// drop its live state. The client registry is already persisted on disk.
	pub fn deactivate(&self) {
		let handle = self.0.active.lock().unwrap().take();
		if let Some(handle) = handle {
			handle.serve_task.abort();
			for (_, session) in handle.controller.sessions.lock().unwrap().drain() {
				session.task.abort();
			}
			for (_, tunnel) in handle.controller.tunnels.lock().unwrap().drain() {
				tunnel.task.abort();
			}
			handle.controller.live.lock().unwrap().clear();
		}
		self.notify_changed();
	}
}

fn load_settings(dir: &std::path::Path) -> AppResult<Settings> {
	match std::fs::read_to_string(dir.join("settings.json")) {
		Ok(raw) => serde_json::from_str(&raw).map_err(|e| AppError::msg(format!("parsing settings.json: {e}"))),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
		Err(e) => Err(AppError::Io(e)),
	}
}
