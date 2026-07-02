//! Agent side of file transfer: serve a [`StreamOpen::Download`] (the agent is the
//! sender, walking a local tree and streaming it out) or a [`StreamOpen::Upload`] (the
//! agent is the receiver, writing an incoming tree to disk). The resumable per-file
//! send/receive engine lives in [`libretether_protocol::transfer`]; this module only
//! enumerates the local tree, drives the manifest, and maps transfer-relative paths to
//! the agent's filesystem.
//!
//! Path safety: an upload's destination paths are built with
//! [`libretether_protocol::transfer::safe_join`], which refuses any `..`/absolute/drive
//! component, so a transfer can never write outside the chosen destination directory.
//! Symlinks in a download tree are reported/skipped rather than followed, to avoid loops.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use libretether_protocol::e2e::{SecureQuicRecv, SecureQuicSend};
use libretether_protocol::frame::{read_frame_capped, write_frame, MAX_CONTROL_FRAME};
use libretether_protocol::transfer::{
	self, DownloadRequest, FileDone, FileHeader, ManifestChunk, TransferManifest, UploadRequest, Walked,
};
use libretether_protocol::LogLevel;
use tokio::io::AsyncWriteExt;

/// Serve a download: the controller wants to pull a file/tree from this agent.
pub async fn serve_download(mut send: SecureQuicSend, mut recv: SecureQuicRecv) {
	if let Err(e) = download(&mut send, &mut recv).await {
		crate::net::log_at(LogLevel::Warn, &format!("download failed: {e:#}"));
	}
	let _ = send.shutdown().await;
}

/// Serve an upload: the controller is pushing a file/tree to this agent.
pub async fn serve_upload(mut send: SecureQuicSend, mut recv: SecureQuicRecv) {
	if let Err(e) = upload(&mut send, &mut recv).await {
		crate::net::log_at(LogLevel::Warn, &format!("upload failed: {e:#}"));
	}
	let _ = send.shutdown().await;
}

async fn download(send: &mut SecureQuicSend, recv: &mut SecureQuicRecv) -> Result<()> {
	let req: DownloadRequest = read_frame_capped(recv, MAX_CONTROL_FRAME).await?;
	crate::net::debug(&format!("download requested: {} (compress={})", req.path, req.compress));
	let compress = req.compress;

	// Resolve + enumerate on the blocking pool; a bad path is reported to the controller
	// (as an `Err` manifest) rather than silently dropping the stream.
	let path = req.path.clone();
	let walked = match tokio::task::spawn_blocking(move || enumerate(&path)).await {
		Ok(Ok(w)) => w,
		Ok(Err(e)) => {
			write_frame(send, &Err::<TransferManifest, String>(e)).await?;
			return Ok(());
		}
		Err(e) => {
			write_frame(send, &Err::<TransferManifest, String>(format!("walk task failed: {e}"))).await?;
			return Ok(());
		}
	};

	let files: Vec<&transfer::WalkItem> = walked.files().collect();
	let manifest = TransferManifest {
		root_name: walked.root_name.clone(),
		root_is_dir: walked.root_is_dir,
		total_files: files.len() as u64,
		total_bytes: walked.total_bytes,
	};
	write_frame(send, &Ok::<_, String>(manifest)).await?;

	// Stream the full entry list (dirs included, so empty ones round-trip) in batches.
	for batch in walked.entries().chunks(transfer::MANIFEST_BATCH) {
		write_frame(send, &ManifestChunk::Entries(batch.to_vec())).await?;
	}
	write_frame(send, &ManifestChunk::End).await?;

	// Then each file, in order: header, then the resumable byte stream.
	for (index, item) in files.iter().enumerate() {
		let header = FileHeader {
			index: index as u64,
			rel: item.entry.rel.clone(),
			size: item.entry.size,
			mtime: item.entry.mtime,
		};
		write_frame(send, &header).await?;
		transfer::send_file(send, recv, &item.abs, item.entry.size, compress, |_| {}).await?;
	}
	crate::net::debug(&format!("download complete: {} file(s)", files.len()));
	Ok(())
}

/// Resolve + enumerate a source path, canonicalizing first so the manifest's absolute
/// paths are stable. Returns a user-facing error string on a bad path.
fn enumerate(path: &str) -> Result<Walked, String> {
	let root = std::fs::canonicalize(path).map_err(|e| format!("{path}: {e}"))?;
	transfer::enumerate(&root).map_err(|e| e.to_string())
}

async fn upload(send: &mut SecureQuicSend, recv: &mut SecureQuicRecv) -> Result<()> {
	let req: UploadRequest = read_frame_capped(recv, MAX_CONTROL_FRAME).await?;
	let manifest: TransferManifest = read_frame_capped(recv, MAX_CONTROL_FRAME).await?;
	crate::net::debug(&format!(
		"upload requested into {} ({} file(s), {} bytes)",
		req.dest_dir, manifest.total_files, manifest.total_bytes
	));

	// Make sure the destination directory exists, then use it as the safe base.
	let dest = PathBuf::from(&req.dest_dir);
	if let Err(e) = std::fs::create_dir_all(&dest) {
		write_frame(send, &Err::<(), String>(format!("{}: {e}", dest.display()))).await?;
		return Ok(());
	}
	let dest = std::fs::canonicalize(&dest).unwrap_or(dest);
	let root_name = manifest.root_name.clone();

	// Consume the streamed manifest, creating directories (empty ones included).
	while let ManifestChunk::Entries(entries) = read_frame_capped::<_, ManifestChunk>(recv, MAX_CONTROL_FRAME).await? {
		for e in entries {
			if e.is_dir {
				if let Some(dir) = transfer::safe_join(&dest, &transfer::under_root(&root_name, &e.rel)) {
					let _ = std::fs::create_dir_all(&dir);
				}
			}
		}
	}
	// Tell the controller we're ready for the files.
	write_frame(send, &Ok::<(), String>(())).await?;

	for _ in 0..manifest.total_files {
		let header: FileHeader = read_frame_capped(recv, MAX_CONTROL_FRAME).await?;
		let target = transfer::safe_join(&dest, &transfer::under_root(&root_name, &header.rel))
			.ok_or_else(|| anyhow!("refusing unsafe transfer path: {}", header.rel))?;
		let committed = transfer::receive_file(send, recv, &target, &header, |_| {}).await?;
		write_frame(
			send,
			&FileDone {
				index: header.index,
				committed,
			},
		)
		.await?;
	}
	crate::net::debug(&format!("upload complete: {} file(s)", manifest.total_files));
	Ok(())
}
