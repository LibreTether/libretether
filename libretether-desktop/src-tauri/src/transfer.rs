//! Controller side of file transfer: drive a queued [`TransferItem`] over a
//! [`StreamOpen::Download`] / [`StreamOpen::Upload`] stream to an agent, surfacing
//! progress to the webview and persisting enough to resume.
//!
//! Mirrors [`crate::session`]: `start` registers a [`TransferHandle`] on the
//! [`ActiveController`] and spawns [`drive`]; cancel/pause abort the task; `finish`
//! only clears a handle if it's still the current one. Unlike a session, transfers are
//! queue-driven — at most one runs per machine at a time (sequential per machine,
//! parallel across machines), and completing one starts the next queued item for that
//! machine. When a machine (re)connects, [`resume_for`] kicks its queued transfers; a
//! periodic [`pump`] is the backstop.
//!
//! The resumable per-file byte engine lives in [`libretether_protocol::transfer`]; this
//! module orchestrates the manifest, maps transfer-relative paths under the chosen
//! local directory, and accounts progress.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use libretether_protocol::e2e::{SecureQuicRecv, SecureQuicSend};
use libretether_protocol::frame::{read_frame_capped, write_frame, MAX_CONTROL_FRAME};
use libretether_protocol::transfer::{
	self, DownloadRequest, FileDone, FileHeader, ManifestChunk, ManifestEntry, TransferManifest, UploadRequest,
};
use libretether_protocol::StreamOpen;
use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter};
use uuid::Uuid;

use crate::state::{ActiveController, AppState, TransferHandle};
use crate::transfer_queue::{Direction, TransferStatus};

static TRANSFER_GEN: AtomicU64 = AtomicU64::new(1);

/// How often the backstop [`pump`] re-checks online machines for queued transfers.
const PUMP_INTERVAL: Duration = Duration::from_secs(15);

/// A drive attempt's failure mode. `Interrupted` (a dropped connection / IO error) is
/// retryable — the item goes back to `Queued` and auto-resumes on the next reconnect or
/// pump tick. `Fatal` (a bad path, an agent-refused request, an unsafe manifest) is
/// terminal — the item goes to `Error` for the user to retry explicitly.
enum DriveError {
	Interrupted(String),
	Fatal(String),
}

fn interrupted<E: std::fmt::Display>(e: E) -> DriveError {
	DriveError::Interrupted(e.to_string())
}

/// Background poller: resume queued transfers to any machine that's currently online.
/// Runs for the life of the active controller (aborted on deactivate).
pub async fn pump(state: AppState, ctrl: Arc<ActiveController>) {
	loop {
		tokio::time::sleep(PUMP_INTERVAL).await;
		let online: Vec<Uuid> = ctrl.live.lock().unwrap().keys().copied().collect();
		for client_id in online {
			start_next_for(&state, &ctrl, client_id);
		}
	}
}

/// A machine just came online (or finished a transfer): start its next queued transfer,
/// if any and if it isn't already running one.
pub fn resume_for(state: &AppState, ctrl: &Arc<ActiveController>, client_id: Uuid) {
	start_next_for(state, ctrl, client_id);
}

/// Start the oldest queued transfer for `client_id`, unless one is already running for
/// that machine (sequential per machine).
fn start_next_for(state: &AppState, ctrl: &Arc<ActiveController>, client_id: Uuid) {
	let busy = ctrl
		.transfers_live
		.lock()
		.unwrap()
		.values()
		.any(|h| h.client_id == client_id);
	if busy {
		return;
	}
	let next = ctrl.transfers.lock().unwrap().queued_for(client_id).into_iter().next();
	if let Some(item_id) = next {
		start(state, ctrl, item_id);
	}
}

/// Register a handle and spawn the driver for a specific queued transfer. No-op if it's
/// already running or no longer in the queue.
pub fn start(state: &AppState, ctrl: &Arc<ActiveController>, item_id: Uuid) {
	let Some(item) = ctrl.transfers.lock().unwrap().get(item_id).cloned() else {
		return;
	};
	if ctrl.transfers_live.lock().unwrap().contains_key(&item_id) {
		return;
	}
	let token = TRANSFER_GEN.fetch_add(1, Ordering::Relaxed);
	ctrl.mutate_transfers(|q| q.set_status(item_id, TransferStatus::Active));
	let app = state.0.app.get().cloned();
	emit_changed(&app);
	crate::logbook::info(
		"transfer",
		&format!(
			"starting {} of '{}' ({})",
			direction_verb(item.direction),
			item.name,
			item.client_id
		),
	);
	let client_id = item.client_id;
	let st = state.clone();
	let c = ctrl.clone();
	let task = tauri::async_runtime::spawn(drive(st, c, item, token, app));
	ctrl.transfers_live
		.lock()
		.unwrap()
		.insert(item_id, TransferHandle { client_id, task, token });
}

async fn drive(
	state: AppState,
	ctrl: Arc<ActiveController>,
	item: crate::transfer_queue::TransferItem,
	token: u64,
	app: Option<AppHandle>,
) {
	let item_id = item.id;
	let client_id = item.client_id;
	match run(&ctrl, &item, &app).await {
		Ok(()) => {
			ctrl.mutate_transfers(|q| {
				q.update(item_id, |t| {
					t.status = TransferStatus::Done;
					t.error = None;
					t.files_done = t.total_files;
					t.bytes_done = t.total_bytes;
				});
			});
			crate::logbook::info("transfer", &format!("transfer {item_id} complete"));
		}
		Err(DriveError::Interrupted(msg)) => {
			crate::logbook::warn("transfer", &format!("transfer {item_id} interrupted: {msg}"));
			// Back to Queued so a reconnect / pump auto-resumes it (re-deriving the offset).
			ctrl.mutate_transfers(|q| q.set_status(item_id, TransferStatus::Queued));
		}
		Err(DriveError::Fatal(msg)) => {
			crate::logbook::warn("transfer", &format!("transfer {item_id} failed: {msg}"));
			ctrl.mutate_transfers(|q| {
				q.update(item_id, |t| {
					t.status = TransferStatus::Error;
					t.error = Some(msg);
				});
			});
		}
	}
	finish(&ctrl, item_id, token);
	emit_changed(&app);
	// Advance the queue for this machine (start the next queued item, if any).
	start_next_for(&state, &ctrl, client_id);
}

/// Open the stream and run the direction-specific protocol.
async fn run(
	ctrl: &ActiveController,
	item: &crate::transfer_queue::TransferItem,
	app: &Option<AppHandle>,
) -> Result<(), DriveError> {
	let link = ctrl
		.connection(item.client_id)
		.ok_or_else(|| DriveError::Interrupted("client is offline".into()))?;
	let open = match item.direction {
		Direction::Download => StreamOpen::Download,
		Direction::Upload => StreamOpen::Upload,
	};
	let (mut send, mut recv) = link
		.open_authenticated(open)
		.await
		.map_err(|e| DriveError::Interrupted(format!("open stream: {e}")))?;
	match item.direction {
		Direction::Download => run_download(ctrl, item, &mut send, &mut recv, app).await,
		Direction::Upload => run_upload(ctrl, item, &mut send, &mut recv, app).await,
	}
}

async fn run_download(
	ctrl: &ActiveController,
	item: &crate::transfer_queue::TransferItem,
	send: &mut SecureQuicSend,
	recv: &mut SecureQuicRecv,
	app: &Option<AppHandle>,
) -> Result<(), DriveError> {
	write_frame(
		send,
		&DownloadRequest {
			path: item.remote_path.clone(),
		},
	)
	.await
	.map_err(interrupted)?;
	// The agent replies with the manifest, or an error string for a bad source path.
	let manifest: Result<TransferManifest, String> =
		read_frame_capped(recv, MAX_CONTROL_FRAME).await.map_err(interrupted)?;
	let manifest = manifest.map_err(DriveError::Fatal)?;

	let mut entries: Vec<ManifestEntry> = Vec::new();
	while let ManifestChunk::Entries(mut e) = read_frame_capped::<_, ManifestChunk>(recv, MAX_CONTROL_FRAME)
		.await
		.map_err(interrupted)?
	{
		entries.append(&mut e);
	}
	set_totals(ctrl, item.id, &manifest, app);

	let base = PathBuf::from(&item.local_path);
	// Create directories up front so empty ones round-trip (files' parents are also
	// created by the receive engine).
	for e in entries.iter().filter(|e| e.is_dir) {
		if let Some(dir) = transfer::safe_join(&base, &transfer::under_root(&manifest.root_name, &e.rel)) {
			let _ = tokio::fs::create_dir_all(&dir).await;
		}
	}

	let mut prog = Progress::new(app.clone(), item.id, manifest.total_files, manifest.total_bytes);
	for _ in 0..manifest.total_files {
		let header: FileHeader = read_frame_capped(recv, MAX_CONTROL_FRAME).await.map_err(interrupted)?;
		let target = transfer::safe_join(&base, &transfer::under_root(&manifest.root_name, &header.rel))
			.ok_or_else(|| DriveError::Fatal(format!("refusing unsafe transfer path: {}", header.rel)))?;
		let committed = transfer::receive_file(send, recv, &target, &header, |b| prog.report(b))
			.await
			.map_err(interrupted)?;
		prog.complete_file(committed);
		persist_progress(ctrl, item.id, &prog);
	}
	Ok(())
}

async fn run_upload(
	ctrl: &ActiveController,
	item: &crate::transfer_queue::TransferItem,
	send: &mut SecureQuicSend,
	recv: &mut SecureQuicRecv,
	app: &Option<AppHandle>,
) -> Result<(), DriveError> {
	// Enumerate the local source on the blocking pool.
	let source = item.local_path.clone();
	let walked = tokio::task::spawn_blocking(move || {
		let root = std::fs::canonicalize(&source).map_err(|e| format!("{source}: {e}"))?;
		transfer::enumerate(&root).map_err(|e| e.to_string())
	})
	.await
	.map_err(|e| DriveError::Fatal(format!("walk task failed: {e}")))?
	.map_err(DriveError::Fatal)?;

	let files: Vec<&transfer::WalkItem> = walked.files().collect();
	let manifest = TransferManifest {
		root_name: walked.root_name.clone(),
		root_is_dir: walked.root_is_dir,
		total_files: files.len() as u64,
		total_bytes: walked.total_bytes,
	};
	write_frame(
		send,
		&UploadRequest {
			dest_dir: item.remote_path.clone(),
		},
	)
	.await
	.map_err(interrupted)?;
	write_frame(send, &manifest).await.map_err(interrupted)?;
	for batch in walked.entries().chunks(transfer::MANIFEST_BATCH) {
		write_frame(send, &ManifestChunk::Entries(batch.to_vec()))
			.await
			.map_err(interrupted)?;
	}
	write_frame(send, &ManifestChunk::End).await.map_err(interrupted)?;

	// The agent readies the destination and acks (or reports a fatal error).
	let ready: Result<(), String> = read_frame_capped(recv, MAX_CONTROL_FRAME).await.map_err(interrupted)?;
	ready.map_err(DriveError::Fatal)?;
	set_totals(ctrl, item.id, &manifest, app);

	let mut prog = Progress::new(app.clone(), item.id, manifest.total_files, manifest.total_bytes);
	for (index, wi) in files.iter().enumerate() {
		let header = FileHeader {
			index: index as u64,
			rel: wi.entry.rel.clone(),
			size: wi.entry.size,
			mtime: wi.entry.mtime,
		};
		write_frame(send, &header).await.map_err(interrupted)?;
		transfer::send_file(send, recv, &wi.abs, wi.entry.size, |b| prog.report(b))
			.await
			.map_err(interrupted)?;
		let done: FileDone = read_frame_capped(recv, MAX_CONTROL_FRAME).await.map_err(interrupted)?;
		prog.complete_file(done.committed);
		persist_progress(ctrl, item.id, &prog);
	}
	Ok(())
}

/// Persist the manifest totals to the queue and refresh the UI list.
fn set_totals(ctrl: &ActiveController, item_id: Uuid, manifest: &TransferManifest, app: &Option<AppHandle>) {
	ctrl.mutate_transfers(|q| {
		q.update(item_id, |t| {
			t.total_files = manifest.total_files;
			t.total_bytes = manifest.total_bytes;
		});
	});
	emit_changed(app);
}

/// Persist file-boundary progress (a resume hint; not per byte).
fn persist_progress(ctrl: &ActiveController, item_id: Uuid, prog: &Progress) {
	ctrl.mutate_transfers(|q| {
		q.update(item_id, |t| {
			t.files_done = prog.files_done;
			t.bytes_done = prog.base_bytes;
		});
	});
}

/// Remove our handle — but only if we're still the current driver for this item (a
/// newer start, or a cancel/pause, may have replaced or removed us).
fn finish(ctrl: &ActiveController, item_id: Uuid, token: u64) {
	let mut live = ctrl.transfers_live.lock().unwrap();
	if live.get(&item_id).map(|h| h.token) == Some(token) {
		live.remove(&item_id);
	}
}

/// Abort a running driver (if any), leaving the queue item's status to the caller.
fn stop_handle(ctrl: &ActiveController, item_id: Uuid) {
	if let Some(handle) = ctrl.transfers_live.lock().unwrap().remove(&item_id) {
		handle.task.abort();
	}
}

/// Pause a transfer: stop the driver and mark it Paused (its `.part` files survive, so
/// resuming continues from the committed offset).
pub fn pause(ctrl: &Arc<ActiveController>, item_id: Uuid) {
	stop_handle(ctrl, item_id);
	ctrl.mutate_transfers(|q| q.set_status(item_id, TransferStatus::Paused));
}

/// Resume a paused/errored transfer: mark it Queued and kick the machine's queue.
pub fn resume(state: &AppState, ctrl: &Arc<ActiveController>, item_id: Uuid) {
	let client = ctrl.transfers.lock().unwrap().get(item_id).map(|t| t.client_id);
	ctrl.mutate_transfers(|q| q.set_status(item_id, TransferStatus::Queued));
	if let Some(client) = client {
		start_next_for(state, ctrl, client);
	}
}

/// Cancel and remove a transfer from the queue. Any partial (`.part`) files are left on
/// disk (a re-enqueue would resume/overwrite them); the machine's queue advances.
pub fn cancel(state: &AppState, ctrl: &Arc<ActiveController>, item_id: Uuid) {
	let client = ctrl.transfers.lock().unwrap().get(item_id).map(|t| t.client_id);
	stop_handle(ctrl, item_id);
	ctrl.mutate_transfers(|q| {
		q.remove(item_id);
	});
	if let Some(client) = client {
		start_next_for(state, ctrl, client);
	}
}

fn direction_verb(d: Direction) -> &'static str {
	match d {
		Direction::Download => "download",
		Direction::Upload => "upload",
	}
}

/// Emit the "the transfer list changed" event so any open queue view reloads.
pub fn emit_changed(app: &Option<AppHandle>) {
	emit(app, "transfers:changed", ());
}

fn emit<P: Serialize + Clone>(app: &Option<AppHandle>, event: &str, payload: P) {
	if let Some(app) = app {
		let _ = app.emit(event, payload);
	}
}

/// Per-transfer progress accounting + throttled event emission. `base_bytes` is the sum
/// over completed files; `report` adds the in-flight file's cumulative bytes.
struct Progress {
	app: Option<AppHandle>,
	item_id: Uuid,
	total_files: u64,
	total_bytes: u64,
	files_done: u64,
	base_bytes: u64,
	last: Instant,
}

impl Progress {
	fn new(app: Option<AppHandle>, item_id: Uuid, total_files: u64, total_bytes: u64) -> Self {
		let mut p = Self {
			app,
			item_id,
			total_files,
			total_bytes,
			files_done: 0,
			base_bytes: 0,
			last: Instant::now(),
		};
		p.emit(0, true);
		p
	}

	/// Report the in-flight file's cumulative bytes (throttled emit).
	fn report(&mut self, file_bytes: u64) {
		let done = self.base_bytes + file_bytes;
		self.emit(done, false);
	}

	/// Account a completed file and emit a definitive progress tick.
	fn complete_file(&mut self, committed: u64) {
		self.base_bytes += committed;
		self.files_done += 1;
		self.emit(self.base_bytes, true);
	}

	fn emit(&mut self, bytes_done: u64, force: bool) {
		if !force && self.last.elapsed() < Duration::from_millis(100) {
			return;
		}
		self.last = Instant::now();
		emit(
			&self.app,
			"transfer:progress",
			json!({
				"id": self.item_id.to_string(),
				"files_done": self.files_done,
				"bytes_done": bytes_done,
				"total_files": self.total_files,
				"total_bytes": self.total_bytes,
			}),
		);
	}
}
