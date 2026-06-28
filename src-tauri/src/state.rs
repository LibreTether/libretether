//! Controller state: persisted identity/certificate + client registry, and the
//! live map of currently-connected agents.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tether_protocol::crypto::Identity;
use tether_protocol::{tls, AgentStatus, SessionClient, DEFAULT_PORT};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::registry::ClientStore;

/// Event emitted whenever the client list or a client's connection state changes.
pub const EVENT_CHANGED: &str = "clients:changed";

#[derive(Debug, Serialize, Deserialize)]
pub struct ControllerConfig {
	pub listen_port: u16,
	/// Base64 Ed25519 seed — the controller's own identity (shown as a fingerprint).
	pub identity_seed: String,
	/// Base64 DER self-signed certificate + PKCS8 key for the QUIC server.
	pub cert_der: String,
	pub key_der: String,
	/// Manual override for the address agents should dial. When unset, the
	/// controller falls back to its Tailscale address.
	#[serde(default)]
	pub advertise_addr: Option<String>,
	/// Optional Tailscale pre-auth key embedded in deploy scripts so clients can
	/// join the tailnet without an interactive login. When unset, deploy scripts
	/// assume a direct connection.
	#[serde(default)]
	pub tailscale_auth_key: Option<String>,
}

impl ControllerConfig {
	fn generate() -> Self {
		let (cert_der, key_der) = tls::self_signed();
		Self {
			listen_port: DEFAULT_PORT,
			identity_seed: Identity::generate().seed_b64(),
			cert_der: B64.encode(cert_der),
			key_der: B64.encode(key_der),
			advertise_addr: None,
			tailscale_auth_key: None,
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
			.map(|id| {
				let pk = id.public_b64();
				pk.chars().take(12).collect()
			})
			.unwrap_or_else(|| "unknown".to_string())
	}
}

/// A live agent connection.
pub struct LiveConn {
	pub conn: quinn::Connection,
	pub status: Option<AgentStatus>,
}

/// A running screen-control session: a channel that pushes input/control into
/// the session writer task, plus the task handle for teardown.
pub struct SessionHandle {
	pub input_tx: tokio::sync::mpsc::UnboundedSender<SessionClient>,
	pub task: tauri::async_runtime::JoinHandle<()>,
}

pub struct Inner {
	pub config_dir: PathBuf,
	pub config: Mutex<ControllerConfig>,
	pub store: Mutex<ClientStore>,
	pub live: Mutex<HashMap<Uuid, LiveConn>>,
	pub sessions: Mutex<HashMap<Uuid, SessionHandle>>,
	pub app: OnceLock<AppHandle>,
}

#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

impl AppState {
	pub fn init(config_dir: PathBuf) -> AppResult<Self> {
		std::fs::create_dir_all(&config_dir)?;
		let config = load_or_create_config(&config_dir)?;
		let store = ClientStore::load(config_dir.join("clients.json"))?;
		Ok(Self(Arc::new(Inner {
			config_dir,
			config: Mutex::new(config),
			store: Mutex::new(store),
			live: Mutex::new(HashMap::new()),
			sessions: Mutex::new(HashMap::new()),
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

	/// Clone out the live connection for a client, if connected.
	pub fn connection(&self, id: Uuid) -> Option<quinn::Connection> {
		self.0.live.lock().unwrap().get(&id).map(|c| c.conn.clone())
	}

	pub fn is_online(&self, id: Uuid) -> bool {
		self.0.live.lock().unwrap().contains_key(&id)
	}

	/// Persist the current controller config to disk.
	pub fn save_config(&self) -> AppResult<()> {
		let raw = {
			let cfg = self.0.config.lock().unwrap();
			serde_json::to_string_pretty(&*cfg)
				.map_err(|e| AppError::msg(format!("serializing controller config: {e}")))?
		};
		std::fs::write(self.0.config_dir.join("controller.json"), raw)?;
		Ok(())
	}
}

fn load_or_create_config(dir: &Path) -> AppResult<ControllerConfig> {
	let path = dir.join("controller.json");
	match std::fs::read_to_string(&path) {
		Ok(raw) => serde_json::from_str(&raw).map_err(|e| AppError::msg(format!("parsing controller.json: {e}"))),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			let config = ControllerConfig::generate();
			let raw = serde_json::to_string_pretty(&config)
				.map_err(|e| AppError::msg(format!("serializing controller config: {e}")))?;
			std::fs::write(&path, raw)?;
			Ok(config)
		}
		Err(e) => Err(AppError::Io(e)),
	}
}
