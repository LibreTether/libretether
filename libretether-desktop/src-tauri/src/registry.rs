//! Persisted record of the machines (clients) this controller manages.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use libretether_protocol::crypto;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientOs {
	Linux,
	Macos,
	Windows,
}

impl ClientOs {
	pub fn as_str(&self) -> &'static str {
		match self {
			ClientOs::Linux => "linux",
			ClientOs::Macos => "macos",
			ClientOs::Windows => "windows",
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Client {
	pub id: Uuid,
	pub name: String,
	pub os: ClientOs,
	pub created_at: u64,
	pub enrolled: bool,
	/// Set once the agent has enrolled — its stable Ed25519 public key.
	pub public_key: Option<String>,
	/// One-time enrollment token, cleared after first successful enrollment.
	pub enrollment_token: Option<String>,
	pub last_seen: Option<u64>,
}

impl Client {
	fn new(name: String, os: ClientOs) -> Self {
		Self {
			id: Uuid::new_v4(),
			name,
			os,
			created_at: now_secs(),
			enrolled: false,
			public_key: None,
			enrollment_token: Some(new_token()),
			last_seen: None,
		}
	}
}

/// File-backed list of clients.
pub struct ClientStore {
	path: PathBuf,
	clients: Vec<Client>,
}

impl ClientStore {
	pub fn load(path: PathBuf) -> AppResult<Self> {
		let clients = match std::fs::read_to_string(&path) {
			Ok(raw) => serde_json::from_str(&raw).map_err(|e| AppError::msg(format!("parsing clients.json: {e}")))?,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
			Err(e) => return Err(AppError::Io(e)),
		};
		Ok(Self { path, clients })
	}

	fn persist(&self) -> AppResult<()> {
		let raw = serde_json::to_string_pretty(&self.clients)
			.map_err(|e| AppError::msg(format!("serializing clients: {e}")))?;
		// Holds one-time enrollment tokens — write owner-only.
		libretether_protocol::secret::write_str(&self.path, &raw)?;
		Ok(())
	}

	pub fn list(&self) -> &[Client] {
		&self.clients
	}

	pub fn create(&mut self, name: String, os: ClientOs) -> AppResult<Client> {
		let client = Client::new(name, os);
		self.clients.push(client.clone());
		self.persist()?;
		Ok(client)
	}

	pub fn get(&self, id: Uuid) -> Option<&Client> {
		self.clients.iter().find(|c| c.id == id)
	}

	pub fn remove(&mut self, id: Uuid) -> AppResult<()> {
		let before = self.clients.len();
		self.clients.retain(|c| c.id != id);
		if self.clients.len() == before {
			return Err(AppError::NotFound);
		}
		self.persist()
	}

	pub fn rename(&mut self, id: Uuid, name: String) -> AppResult<()> {
		let client = self.clients.iter_mut().find(|c| c.id == id).ok_or(AppError::NotFound)?;
		client.name = name;
		self.persist()
	}

	/// Regenerate the one-time enrollment token (e.g. to re-deploy a client).
	pub fn reset_token(&mut self, id: Uuid) -> AppResult<String> {
		let client = self.clients.iter_mut().find(|c| c.id == id).ok_or(AppError::NotFound)?;
		let token = new_token();
		client.enrollment_token = Some(token.clone());
		client.enrolled = false;
		client.public_key = None;
		self.persist()?;
		Ok(token)
	}

	/// Resolve the agent in an incoming handshake to a client id, enrolling it
	/// on first contact. Returns the client id when accepted.
	pub fn authenticate(&mut self, token: Option<&str>, public_key: &str) -> Option<Uuid> {
		// First connect: match the one-time token and bind the public key.
		if let Some(token) = token {
			if let Some(client) = self
				.clients
				.iter_mut()
				.find(|c| c.enrollment_token.as_deref().is_some_and(|t| crypto::ct_eq(t, token)))
			{
				client.enrolled = true;
				client.public_key = Some(public_key.to_string());
				client.enrollment_token = None;
				client.last_seen = Some(now_secs());
				let id = client.id;
				let _ = self.persist();
				return Some(id);
			}
		}
		// Reconnect: match the bound public key.
		if let Some(client) = self
			.clients
			.iter_mut()
			.find(|c| c.public_key.as_deref() == Some(public_key))
		{
			client.last_seen = Some(now_secs());
			let id = client.id;
			let _ = self.persist();
			return Some(id);
		}
		None
	}

	pub fn id_for_pubkey(&self, public_key: &str) -> Option<Uuid> {
		self.clients
			.iter()
			.find(|c| c.public_key.as_deref() == Some(public_key))
			.map(|c| c.id)
	}

	pub fn touch_seen(&mut self, id: Uuid) {
		if let Some(client) = self.clients.iter_mut().find(|c| c.id == id) {
			client.last_seen = Some(now_secs());
			let _ = self.persist();
		}
	}
}

pub fn now_secs() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

/// A reasonably long, URL-safe one-time token.
fn new_token() -> String {
	format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
