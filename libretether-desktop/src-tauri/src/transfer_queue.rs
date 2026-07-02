//! Persisted file-transfer queue for the active controller.
//!
//! One entry per *selection* the user enqueued — a file or a whole directory tree, in
//! either direction. The queue is file-backed (like the client [`crate::registry`]) so
//! it survives a controller restart: on the next launch it rehydrates and any transfer
//! whose machine is online auto-resumes (see [`crate::transfer`]).
//!
//! Progress (`files_done` / `bytes_done`) is persisted only at file boundaries and on
//! status changes, not per byte — it's a display hint. The authoritative resume offset
//! is always re-derived from the receiver's `.part` file on (re)connect, so a
//! slightly-stale persisted count never causes data loss or duplication.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::registry::now_secs;

/// Which way a transfer moves relative to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
	/// Agent → controller (pull a file/tree from the machine).
	Download,
	/// Controller → agent (push a file/tree to the machine).
	Upload,
}

/// Lifecycle of a queued transfer. `Queued` covers both "not started yet" and
/// "interrupted, waiting to auto-resume"; `Error` is a terminal failure the user must
/// retry explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatus {
	Queued,
	Active,
	Paused,
	Done,
	Error,
}

/// One enqueued transfer. `remote_path` is the source (download) or destination
/// directory (upload) on the agent; `local_path` is the destination directory
/// (download) or source (upload) on the controller host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferItem {
	pub id: Uuid,
	pub client_id: Uuid,
	pub direction: Direction,
	pub remote_path: String,
	pub local_path: String,
	/// Whether the selection is a directory tree (vs a single file).
	pub is_dir: bool,
	/// Display name — the transfer root's base name.
	pub name: String,
	pub total_files: u64,
	pub total_bytes: u64,
	pub files_done: u64,
	pub bytes_done: u64,
	pub status: TransferStatus,
	pub error: Option<String>,
	pub created_at: u64,
	pub updated_at: u64,
}

impl TransferItem {
	pub fn new(
		client_id: Uuid,
		direction: Direction,
		remote_path: String,
		local_path: String,
		is_dir: bool,
		name: String,
	) -> Self {
		let now = now_secs();
		Self {
			id: Uuid::new_v4(),
			client_id,
			direction,
			remote_path,
			local_path,
			is_dir,
			name,
			total_files: 0,
			total_bytes: 0,
			files_done: 0,
			bytes_done: 0,
			status: TransferStatus::Queued,
			error: None,
			created_at: now,
			updated_at: now,
		}
	}
}

/// File-backed list of transfers, mirroring [`crate::registry::ClientStore`]: the
/// mutating methods change memory only; the caller persists a [`Self::snapshot`] via
/// [`crate::state::ActiveController::mutate_transfers`], which writes after releasing
/// the lock.
pub struct TransferQueue {
	path: PathBuf,
	items: Vec<TransferItem>,
}

impl TransferQueue {
	pub fn load(path: PathBuf) -> AppResult<Self> {
		let items = match std::fs::read_to_string(&path) {
			Ok(raw) => serde_json::from_str(&raw).map_err(|e| AppError::msg(format!("parsing transfers.json: {e}")))?,
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
			Err(e) => return Err(AppError::Io(e)),
		};
		Ok(Self { path, items })
	}

	/// Serialize the queue to `(path, json)` for an owner-only write (see
	/// [`crate::registry::ClientStore::snapshot`] for why the write happens outside the lock).
	pub fn snapshot(&self) -> AppResult<(PathBuf, String)> {
		let raw = serde_json::to_string_pretty(&self.items)
			.map_err(|e| AppError::msg(format!("serializing transfers: {e}")))?;
		Ok((self.path.clone(), raw))
	}

	pub fn list(&self) -> &[TransferItem] {
		&self.items
	}

	pub fn get(&self, id: Uuid) -> Option<&TransferItem> {
		self.items.iter().find(|t| t.id == id)
	}

	pub fn enqueue(&mut self, item: TransferItem) {
		self.items.push(item);
	}

	pub fn remove(&mut self, id: Uuid) -> bool {
		let before = self.items.len();
		self.items.retain(|t| t.id != id);
		self.items.len() != before
	}

	/// Apply `f` to the item with `id` (if present) and bump its `updated_at`. Returns
	/// whether an item matched.
	pub fn update(&mut self, id: Uuid, f: impl FnOnce(&mut TransferItem)) -> bool {
		if let Some(item) = self.items.iter_mut().find(|t| t.id == id) {
			f(item);
			item.updated_at = now_secs();
			true
		} else {
			false
		}
	}

	pub fn set_status(&mut self, id: Uuid, status: TransferStatus) {
		self.update(id, |t| t.status = status);
	}

	/// Demote any `Active` items to `Queued` — used on load, since an `Active` item was
	/// mid-flight when the controller last exited and must be restarted, not resumed
	/// in place.
	pub fn requeue_active(&mut self) {
		for item in &mut self.items {
			if item.status == TransferStatus::Active {
				item.status = TransferStatus::Queued;
			}
		}
	}

	/// IDs of transfers for `client_id` that are waiting to run (freshly queued or
	/// interrupted), oldest first — the auto-resume work list when a machine reconnects.
	pub fn queued_for(&self, client_id: Uuid) -> Vec<Uuid> {
		let mut pending: Vec<&TransferItem> = self
			.items
			.iter()
			.filter(|t| t.client_id == client_id && t.status == TransferStatus::Queued)
			.collect();
		pending.sort_by_key(|t| t.created_at);
		pending.into_iter().map(|t| t.id).collect()
	}
}
