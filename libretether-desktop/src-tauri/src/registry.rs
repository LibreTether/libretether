//! Persisted record of the machines (clients) this controller manages.

use std::path::PathBuf;

pub use libretether_common::now_secs;
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

	/// Serialize the registry to `(path, json)` for an owner-only write. The
	/// mutating methods below deliberately do **not** write to disk themselves —
	/// callers go through [`ActiveController::mutate_store`], which takes this
	/// snapshot under the store lock and then writes it *after* releasing the lock,
	/// so the blocking fsync never serializes every other store access.
	pub fn snapshot(&self) -> AppResult<(PathBuf, String)> {
		let raw = serde_json::to_string_pretty(&self.clients)
			.map_err(|e| AppError::msg(format!("serializing clients: {e}")))?;
		Ok((self.path.clone(), raw))
	}

	/// Write the registry to disk now (owner-only). Used in tests and any explicit
	/// save; production mutations persist via [`ActiveController::mutate_store`].
	#[cfg(test)]
	fn persist(&self) -> AppResult<()> {
		let (path, raw) = self.snapshot()?;
		// Holds one-time enrollment tokens — write owner-only.
		libretether_protocol::secret::write_str(path, &raw)?;
		Ok(())
	}

	pub fn list(&self) -> &[Client] {
		&self.clients
	}

	pub fn create(&mut self, name: String, os: ClientOs) -> Client {
		let client = Client::new(name, os);
		self.clients.push(client.clone());
		client
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
		Ok(())
	}

	pub fn rename(&mut self, id: Uuid, name: String) -> AppResult<()> {
		let client = self.clients.iter_mut().find(|c| c.id == id).ok_or(AppError::NotFound)?;
		client.name = name;
		Ok(())
	}

	/// Regenerate the one-time enrollment token (e.g. to re-deploy a client).
	pub fn reset_token(&mut self, id: Uuid) -> AppResult<String> {
		let client = self.clients.iter_mut().find(|c| c.id == id).ok_or(AppError::NotFound)?;
		let token = new_token();
		client.enrollment_token = Some(token.clone());
		client.enrolled = false;
		client.public_key = None;
		Ok(token)
	}

	/// Resolve the agent in an incoming handshake to a client id, enrolling it
	/// on first contact. Returns the client id when accepted. Mutates in memory
	/// only — the caller persists the token-burn / last-seen update.
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
				return Some(client.id);
			}
		}
		// Reconnect: match the bound public key.
		if let Some(client) = self
			.clients
			.iter_mut()
			.find(|c| c.public_key.as_deref() == Some(public_key))
		{
			client.last_seen = Some(now_secs());
			return Some(client.id);
		}
		None
	}

	pub fn id_for_pubkey(&self, public_key: &str) -> Option<Uuid> {
		self.clients
			.iter()
			.find(|c| c.public_key.as_deref() == Some(public_key))
			.map(|c| c.id)
	}

	/// Update a client's last-seen timestamp. Returns whether a client matched (so
	/// the caller only persists when something actually changed).
	pub fn touch_seen(&mut self, id: Uuid) -> bool {
		if let Some(client) = self.clients.iter_mut().find(|c| c.id == id) {
			client.last_seen = Some(now_secs());
			true
		} else {
			false
		}
	}
}

/// A reasonably long, URL-safe one-time token.
fn new_token() -> String {
	format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A unique temp path so parallel tests don't clobber each other's store file.
	fn temp_store() -> ClientStore {
		use std::sync::atomic::{AtomicU32, Ordering};
		static N: AtomicU32 = AtomicU32::new(0);
		let path = std::env::temp_dir().join(format!(
			"lt-store-{}-{}.json",
			std::process::id(),
			N.fetch_add(1, Ordering::Relaxed)
		));
		let _ = std::fs::remove_file(&path);
		ClientStore::load(path).unwrap()
	}

	#[test]
	fn enroll_binds_key_burns_token_then_reconnects_by_key() {
		let mut store = temp_store();
		let client = store.create("box".into(), ClientOs::Linux);
		let token = client.enrollment_token.clone().unwrap();
		assert!(!client.enrolled && client.public_key.is_none());

		// First connect with the one-time token binds the key and burns the token.
		let id = store.authenticate(Some(&token), "PUBKEY").expect("token enrolls");
		assert_eq!(id, client.id);
		let bound = store.get(id).unwrap();
		assert!(bound.enrolled);
		assert_eq!(bound.public_key.as_deref(), Some("PUBKEY"));
		assert!(bound.enrollment_token.is_none());

		// The token is single-use: presenting it again is rejected.
		assert!(store.authenticate(Some(&token), "OTHERKEY").is_none());
		// Reconnect is by bound key.
		assert_eq!(store.authenticate(None, "PUBKEY"), Some(id));
		// An unknown key is rejected.
		assert!(store.authenticate(None, "UNKNOWN").is_none());

		let _ = std::fs::remove_file(&store.path);
	}

	#[test]
	fn reset_token_revokes_the_old_key_and_issues_a_new_token() {
		let mut store = temp_store();
		let client = store.create("box".into(), ClientOs::Linux);
		let first = client.enrollment_token.clone().unwrap();
		store.authenticate(Some(&first), "PUBKEY").unwrap();

		let new = store.reset_token(client.id).unwrap();
		assert_ne!(new, first);
		// Old key no longer authenticates; the new token does and can rebind.
		assert!(store.authenticate(None, "PUBKEY").is_none());
		assert_eq!(store.authenticate(Some(&new), "NEWKEY"), Some(client.id));

		let _ = std::fs::remove_file(&store.path);
	}

	/// A fresh, unique store path (the file does not exist yet).
	fn fresh_path() -> PathBuf {
		use std::sync::atomic::{AtomicU32, Ordering};
		static N: AtomicU32 = AtomicU32::new(0);
		let p = std::env::temp_dir().join(format!(
			"lt-store-rt-{}-{}.json",
			std::process::id(),
			N.fetch_add(1, Ordering::Relaxed)
		));
		let _ = std::fs::remove_file(&p);
		p
	}

	#[test]
	fn enrolled_state_survives_a_save_load_cycle() {
		// The registry holds enrollment tokens + key bindings; it must round-trip
		// through disk intact (this is what the atomic write protects from torn writes).
		let path = fresh_path();
		let id = {
			let mut store = ClientStore::load(path.clone()).unwrap();
			let c = store.create("box".into(), ClientOs::Linux);
			let token = c.enrollment_token.clone().unwrap();
			store.authenticate(Some(&token), "PUBKEY");
			// Mutations are now in-memory only; persist explicitly before reloading
			// (production code persists via ActiveController::mutate_store).
			store.persist().unwrap();
			c.id
		};
		// A brand-new store loaded from the same file sees the enrolled client.
		let reloaded = ClientStore::load(path.clone()).unwrap();
		let c = reloaded.get(id).expect("client persisted");
		assert!(c.enrolled);
		assert_eq!(c.public_key.as_deref(), Some("PUBKEY"));
		assert!(
			c.enrollment_token.is_none(),
			"the burned token stays burned across reloads"
		);
		let _ = std::fs::remove_file(&path);
	}

	#[test]
	fn loading_a_corrupt_store_fails_loudly_rather_than_emptying() {
		// A garbled file must surface a clear parse error (fail closed), not be
		// silently treated as an empty registry that drops every enrolled machine.
		let path = fresh_path();
		std::fs::write(&path, "{ this is not valid json").unwrap();
		let err = match ClientStore::load(path.clone()) {
			Err(e) => e,
			Ok(_) => panic!("a corrupt store must fail to load, not parse as empty"),
		};
		assert!(
			format!("{err}").contains("clients.json"),
			"expected a parse error naming the file, got: {err}"
		);
		let _ = std::fs::remove_file(&path);
	}
}
