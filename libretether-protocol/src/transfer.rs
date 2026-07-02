//! Wire format for file transfer (the [`StreamOpen::Download`] /
//! [`StreamOpen::Upload`] streams).
//!
//! A transfer moves one *selection* — a single file or a whole directory tree —
//! over a dedicated bidirectional stream. Like every non-handshake stream it rides
//! the end-to-end AEAD record layer, so the framing here is oblivious to encryption
//! (a relay only ever forwards ciphertext).
//!
//! ## Exchange
//!
//! Both directions share the same shape; only who sends the bytes differs. The
//! **sender** is the side that has the source (the agent for a download, the
//! controller for an upload); the **receiver** is the side that writes `.part`
//! files and owns durability.
//!
//! 1. The controller writes a [`DownloadRequest`] / [`UploadRequest`] header
//!    (JSON frame).
//! 2. The sender writes a [`TransferManifest`] (root metadata + totals) followed by
//!    one or more [`ManifestChunk::Entries`] frames and a final [`ManifestChunk::End`].
//!    Batching keeps a huge tree from ever buffering as one frame.
//! 3. For each manifest file, in order:
//!    - the sender writes a [`FileHeader`];
//!    - the receiver replies with a [`ResumeReply`] saying how many leading bytes it
//!      already durably holds (see resume, below);
//!    - the sender seeks its source to that offset and streams the remaining bytes as
//!      length-delimited byte chunks (see [`write_chunk`] / [`read_chunk`]),
//!      terminated by an empty chunk ([`write_eof`]);
//!    - for an **upload**, the receiving agent then commits (fsync + atomic rename +
//!      set mtime) and writes a [`FileDone`] so the controller can advance progress
//!      and move to the next file. A download's receiver is the controller itself, so
//!      it needs no such ack.
//!
//! Every JSON frame uses [`crate::frame`] (`u32` length + JSON); byte chunks use the
//! same `u32` length prefix, so the whole stream is a uniform sequence of
//! length-delimited messages whose meaning follows the state machine above.
//!
//! ## Resume
//!
//! The receiver owns durability: file bytes land in a `<target>.part` file (with a
//! `<target>.part.meta` sidecar recording the source `{size, mtime}`). On a fresh or
//! resumed transfer the receiver reports, per file, how much it can keep via
//! [`ResumeReply::have_bytes`]:
//!
//! - the final target already exists with a matching size+mtime → `have_bytes == size`
//!   (the file is complete; the sender streams only the empty terminator and the
//!   receiver skips it);
//! - a `.part` whose sidecar matches the incoming header exists → `have_bytes` is its
//!   length (resume from there);
//! - otherwise `have_bytes == 0` (start fresh; a stale `.part` is discarded).
//!
//! Because the *receiver* is always the authority on the resume offset, a transfer
//! survives a network blip or a full restart on either side: the queue's persisted
//! byte counts are only a display hint — the true offset is re-derived here on every
//! (re)connect.

use std::io::SeekFrom;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use crate::frame::{read_frame_capped, write_frame, MAX_CONTROL_FRAME, MAX_FRAME};
use crate::{DirEntry, DirListing, EntryKind};

/// Byte-chunk size the sender uses for file data. Comfortably under [`MAX_FRAME`],
/// large enough to amortize per-chunk framing/AEAD overhead.
pub const CHUNK_SIZE: usize = 512 * 1024;

/// Controller → agent: the first frame after [`StreamOpen::Download`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
	/// Absolute source path on the agent host — a file or a directory.
	pub path: String,
}

/// Controller → agent: the first frame after [`StreamOpen::Upload`]. The controller
/// enumerates its own local tree, so it (not the agent) sends the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadRequest {
	/// Absolute destination **directory** on the agent host; the transfer's root
	/// (`TransferManifest::root_name`) is created under it.
	pub dest_dir: String,
}

/// Sender → receiver: the transfer preamble, before the streamed entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferManifest {
	/// Base name of the transfer root — the directory name, or the single file's
	/// name. The receiver creates/uses this as the top-level folder or file.
	pub root_name: String,
	/// True when the root is a directory (a recursive tree); false for a single file.
	pub root_is_dir: bool,
	/// Totals across every entry, for progress denominators.
	pub total_files: u64,
	pub total_bytes: u64,
}

/// One file or directory in the manifest, with a path relative to the transfer root.
/// `rel` always uses forward slashes on the wire; the receiver re-roots and
/// normalizes it (rejecting `..` / absolute components).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
	pub rel: String,
	pub size: u64,
	pub mtime: Option<u64>,
	/// True for a directory that must be created even though it carries no file
	/// bytes, so an empty subtree round-trips.
	#[serde(default)]
	pub is_dir: bool,
}

/// Streamed manifest body: entries arrive in one or more `Entries` frames (each kept
/// well under the control-frame cap), then `End`. A million-file tree streams as many
/// small frames rather than one giant allocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "c", rename_all = "snake_case")]
pub enum ManifestChunk {
	Entries(Vec<ManifestEntry>),
	End,
}

/// How many entries to pack into one [`ManifestChunk::Entries`] frame. ~1000 entries
/// of a few hundred bytes each stays comfortably under [`crate::frame::MAX_CONTROL_FRAME`].
pub const MANIFEST_BATCH: usize = 1000;

/// Sender → receiver: the per-file preamble before that file's byte chunks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHeader {
	/// 0-based index into the manifest order, so both ends agree which file this is.
	pub index: u64,
	pub rel: String,
	pub size: u64,
	pub mtime: Option<u64>,
}

/// Receiver → sender, in reply to a [`FileHeader`]: the number of leading bytes the
/// receiver already durably holds for this file. The sender seeks its source to this
/// offset and streams the remainder. `have_bytes == size` means "I already have the
/// whole file" (skip it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeReply {
	pub have_bytes: u64,
}

/// Agent → controller (upload only): the receiver confirms a file is fully committed
/// (fsync'd + atomically renamed into place), so the controller can advance progress
/// and proceed to the next file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDone {
	pub index: u64,
	pub committed: u64,
}

fn invalid(msg: impl Into<String>) -> std::io::Error {
	std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into())
}

/// Write one length-delimited byte chunk of file data. `data` must be non-empty —
/// an empty chunk is the end-of-file marker written by [`write_eof`].
pub async fn write_chunk<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> std::io::Result<()> {
	if data.len() as u64 > MAX_FRAME as u64 {
		return Err(invalid(format!("transfer chunk too large: {} bytes", data.len())));
	}
	w.write_all(&(data.len() as u32).to_be_bytes()).await?;
	w.write_all(data).await?;
	w.flush().await?;
	Ok(())
}

/// Write the end-of-file marker (a zero-length chunk) after a file's byte chunks.
pub async fn write_eof<W: AsyncWrite + Unpin>(w: &mut W) -> std::io::Result<()> {
	w.write_all(&0u32.to_be_bytes()).await?;
	w.flush().await?;
	Ok(())
}

/// Read one length-delimited byte chunk. An empty (zero-length) result is the
/// end-of-file marker: the sender only ever writes non-empty data chunks followed by
/// a single empty terminator, so an empty read is unambiguous.
pub async fn read_chunk<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
	let mut len = [0u8; 4];
	r.read_exact(&mut len).await?;
	let n = u32::from_be_bytes(len);
	if n > MAX_FRAME {
		return Err(invalid(format!(
			"transfer chunk too large: {n} bytes (max {MAX_FRAME})"
		)));
	}
	let mut buf = vec![0u8; n as usize];
	r.read_exact(&mut buf).await?;
	Ok(buf)
}

/// fsync every this many received bytes, so a crash loses at most this much of an
/// in-flight file (and resume continues from the last synced boundary).
const SYNC_EVERY: u64 = 4 * 1024 * 1024;

/// Join a forward-slash, transfer-relative path under `base`, rejecting anything
/// that would escape it: `..`, an absolute path, a drive/root prefix, or an embedded
/// path separator. Empty and `.` components are skipped. Returns `None` on an unsafe
/// path so the receiver can refuse it (the transfer is within-trust, but a malformed
/// or hostile `rel` must never write outside the chosen destination).
pub fn safe_join(base: &Path, rel: &str) -> Option<PathBuf> {
	let mut out = base.to_path_buf();
	for part in rel.split('/') {
		if part.is_empty() || part == "." {
			continue;
		}
		if part == ".." {
			return None;
		}
		// A single plain file name has exactly one `Normal` component and nothing else;
		// a separator, drive prefix (`C:`) or root makes it more than that — reject it.
		let p = Path::new(part);
		let mut comps = p.components();
		match (comps.next(), comps.next()) {
			(Some(Component::Normal(c)), None) => out.push(c),
			_ => return None,
		}
	}
	Some(out)
}

/// The `<target>.part` staging file the receiver appends to until a file completes.
pub fn part_path(target: &Path) -> PathBuf {
	let mut s = target.as_os_str().to_owned();
	s.push(".part");
	PathBuf::from(s)
}

/// The `<target>.part.meta` sidecar recording the source `{size, mtime}` a partial was
/// started against, so a resume can tell a still-valid partial from a stale one.
pub fn meta_path(target: &Path) -> PathBuf {
	let mut s = target.as_os_str().to_owned();
	s.push(".part.meta");
	PathBuf::from(s)
}

#[derive(Serialize, Deserialize)]
struct PartMeta {
	size: u64,
	mtime: Option<u64>,
}

fn mtime_secs(m: &std::fs::Metadata) -> Option<u64> {
	m.modified()
		.ok()
		.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
		.map(|d| d.as_secs())
}

/// Whether an existing file matches the header (same size, and same mtime when the
/// header carries one) — i.e. we already hold this exact file.
fn matches(m: &std::fs::Metadata, header: &FileHeader) -> bool {
	m.len() == header.size && header.mtime.map(|hm| mtime_secs(m) == Some(hm)).unwrap_or(true)
}

/// Best-effort set of a file's modification time from Unix seconds. A failure is
/// non-fatal (the bytes are already correct); mtime is a nicety, not correctness.
fn set_mtime(path: &Path, secs: u64) {
	let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
	if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
		let _ = f.set_modified(when);
	}
}

/// Decide how many leading bytes of `target` the receiver can keep for this file,
/// preparing disk state as a side effect: a complete final file → its full size; a
/// valid matching `.part` → its length (resume); otherwise 0, discarding any stale
/// partial and (re)writing the meta sidecar so the fresh partial is self-describing.
enum Resume {
	/// The final file already exists and matches — nothing to transfer (and any stale
	/// `.part` must be left untouched, never renamed over the good file).
	Complete,
	/// Append to `<target>.part` starting from this many bytes (0 = fresh), then commit.
	Append(u64),
}

async fn resume_decision(target: &Path, part: &Path, meta: &Path, header: &FileHeader) -> std::io::Result<Resume> {
	// 1. The final file is already here and matches — nothing to transfer.
	if let Ok(m) = tokio::fs::metadata(target).await {
		if m.is_file() && matches(&m, header) {
			return Ok(Resume::Complete);
		}
	}
	// 2. A partial started against this exact source — resume from its length (an
	// `== size` length is a crash between the last write and the rename; we still take
	// the append path so it gets committed).
	if let Ok(raw) = tokio::fs::read(meta).await {
		if let Ok(pm) = serde_json::from_slice::<PartMeta>(&raw) {
			if pm.size == header.size && pm.mtime == header.mtime {
				if let Ok(m) = tokio::fs::metadata(part).await {
					if m.len() <= header.size {
						return Ok(Resume::Append(m.len()));
					}
				}
			}
		}
	}
	// 3. Start fresh: drop any stale partial + meta, ensure the parent dir, and write a
	// meta sidecar describing what this partial is for.
	let _ = tokio::fs::remove_file(part).await;
	if let Some(parent) = target.parent() {
		tokio::fs::create_dir_all(parent).await?;
	}
	let raw = serde_json::to_vec(&PartMeta {
		size: header.size,
		mtime: header.mtime,
	})?;
	tokio::fs::write(meta, raw).await?;
	Ok(Resume::Append(0))
}

/// Sender side of one file (used by the agent for a download and the controller for an
/// upload): read the receiver's [`ResumeReply`], seek `source` to that offset, and
/// stream the remaining bytes as chunks + a final EOF. `on_progress` is called with the
/// cumulative bytes sent (including the resumed prefix) after each chunk.
pub async fn send_file<S, R>(
	send: &mut S,
	recv: &mut R,
	source: &Path,
	size: u64,
	mut on_progress: impl FnMut(u64),
) -> std::io::Result<()>
where
	S: AsyncWrite + Unpin,
	R: AsyncRead + Unpin,
{
	let resume: ResumeReply = read_frame_capped(recv, MAX_CONTROL_FRAME).await?;
	let start = resume.have_bytes.min(size);
	on_progress(start);
	if start >= size {
		return write_eof(send).await;
	}
	let mut f = tokio::fs::File::open(source).await?;
	if start > 0 {
		f.seek(SeekFrom::Start(start)).await?;
	}
	let mut sent = start;
	let mut buf = vec![0u8; CHUNK_SIZE];
	loop {
		let n = f.read(&mut buf).await?;
		if n == 0 {
			break;
		}
		write_chunk(send, &buf[..n]).await?;
		sent += n as u64;
		on_progress(sent);
	}
	write_eof(send).await
}

/// Receiver side of one file (used by the controller for a download and the agent for an
/// upload): decide the resume offset, tell the sender via [`ResumeReply`], append the
/// streamed bytes to `<target>.part` (fsyncing periodically), then commit atomically —
/// fsync, rename `.part` → `target`, set mtime, drop the meta sidecar. Returns the final
/// byte count. `on_progress` is called with the cumulative committed-to-`.part` bytes.
pub async fn receive_file<S, R>(
	send: &mut S,
	recv: &mut R,
	target: &Path,
	header: &FileHeader,
	mut on_progress: impl FnMut(u64),
) -> std::io::Result<u64>
where
	S: AsyncWrite + Unpin,
	R: AsyncRead + Unpin,
{
	let part = part_path(target);
	let meta = meta_path(target);
	let start = match resume_decision(target, &part, &meta, header).await? {
		// Already have the exact file: report the full size (the sender streams only the
		// EOF marker), consume it, and clean up any stale partial without touching the
		// good final file.
		Resume::Complete => {
			write_frame(
				send,
				&ResumeReply {
					have_bytes: header.size,
				},
			)
			.await?;
			on_progress(header.size);
			let tail = read_chunk(recv).await?;
			if !tail.is_empty() {
				return Err(std::io::Error::new(
					std::io::ErrorKind::InvalidData,
					"expected end-of-file marker for an already-complete file",
				));
			}
			let _ = tokio::fs::remove_file(&part).await;
			let _ = tokio::fs::remove_file(&meta).await;
			return Ok(header.size);
		}
		Resume::Append(off) => {
			write_frame(send, &ResumeReply { have_bytes: off }).await?;
			on_progress(off);
			off
		}
	};

	if let Some(parent) = part.parent() {
		tokio::fs::create_dir_all(parent).await?;
	}
	let mut f = tokio::fs::OpenOptions::new()
		.create(true)
		.append(true)
		.open(&part)
		.await?;
	let mut written = start;
	let mut since_sync = 0u64;
	loop {
		let chunk = read_chunk(recv).await?;
		if chunk.is_empty() {
			break;
		}
		f.write_all(&chunk).await?;
		written += chunk.len() as u64;
		since_sync += chunk.len() as u64;
		if since_sync >= SYNC_EVERY {
			f.sync_data().await?;
			since_sync = 0;
		}
		on_progress(written);
	}
	f.sync_data().await?;
	drop(f);
	tokio::fs::rename(&part, target).await?;
	if let Some(mtime) = header.mtime {
		set_mtime(target, mtime);
	}
	let _ = tokio::fs::remove_file(&meta).await;
	Ok(written)
}

// -------------------------------------------------------------------- browsing

/// List a directory for the file-transfer browser, or (with `path == None`) seed the
/// browser with the home directory and the filesystem roots. Shared by the agent (over
/// the wire) and the controller (its own host), so the two panes are identical. The
/// blocking `std::fs` work should run on a blocking task.
pub fn browse(path: Option<&str>) -> std::io::Result<DirListing> {
	match path {
		Some(p) => {
			let (path, parent, entries) = list_one(Path::new(p))?;
			Ok(DirListing {
				path,
				parent,
				roots: Vec::new(),
				entries,
			})
		}
		None => {
			let home =
				dirs_home().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory"))?;
			let (path, parent, entries) = list_one(&home)?;
			Ok(DirListing {
				path,
				parent,
				roots: fs_roots(&home),
				entries,
			})
		}
	}
}

/// Resolve + read a single directory, classifying symlinks rather than following them,
/// and sorting directories first then case-insensitively by name.
fn list_one(dir: &Path) -> std::io::Result<(String, Option<String>, Vec<DirEntry>)> {
	let canon = std::fs::canonicalize(dir)?;
	let mut entries = Vec::new();
	for entry in std::fs::read_dir(&canon)? {
		let Ok(entry) = entry else { continue };
		let name = entry.file_name().to_string_lossy().into_owned();
		let kind = match entry.file_type() {
			Ok(ft) if ft.is_dir() => EntryKind::Dir,
			Ok(ft) if ft.is_symlink() => EntryKind::Symlink,
			Ok(ft) if ft.is_file() => EntryKind::File,
			_ => EntryKind::Other,
		};
		// `DirEntry::metadata` does not traverse symlinks, so a symlink reports its own
		// (link) size/mtime rather than the target's.
		let meta = entry.metadata().ok();
		let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
		let mtime = meta.as_ref().and_then(mtime_secs);
		entries.push(DirEntry {
			name,
			kind,
			size,
			mtime,
		});
	}
	entries.sort_by(|a, b| {
		let rank = |k: EntryKind| if k == EntryKind::Dir { 0 } else { 1 };
		rank(a.kind)
			.cmp(&rank(b.kind))
			.then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
	});
	let path = canon.to_string_lossy().into_owned();
	let parent = canon.parent().map(|p| p.to_string_lossy().into_owned());
	Ok((path, parent, entries))
}

/// Filesystem roots to seed the browser: the home directory first, then `/` on unix or
/// each present drive letter on Windows.
fn fs_roots(home: &Path) -> Vec<String> {
	let mut roots = vec![home.to_string_lossy().into_owned()];
	#[cfg(windows)]
	{
		for letter in b'A'..=b'Z' {
			let drive = format!("{}:\\", letter as char);
			if Path::new(&drive).exists() {
				roots.push(drive);
			}
		}
	}
	#[cfg(not(windows))]
	{
		roots.push("/".to_string());
	}
	roots
}

/// The user's home directory. Kept local so this module doesn't depend on the `dirs`
/// crate: `$HOME` on unix, `%USERPROFILE%` on Windows.
fn dirs_home() -> Option<PathBuf> {
	#[cfg(windows)]
	{
		std::env::var_os("USERPROFILE").map(PathBuf::from)
	}
	#[cfg(not(windows))]
	{
		std::env::var_os("HOME").map(PathBuf::from)
	}
}

// ------------------------------------------------------------------ enumeration

/// One walked entry: its manifest form plus, for a file, its absolute source path.
pub struct WalkItem {
	pub entry: ManifestEntry,
	/// The file's real path on disk (the directory's own path for a dir entry, unused).
	pub abs: PathBuf,
}

/// The result of walking a selection: the root's display name and kind, every entry
/// (directories included, so empty ones round-trip), and the total byte count.
pub struct Walked {
	pub root_name: String,
	pub root_is_dir: bool,
	pub items: Vec<WalkItem>,
	pub total_bytes: u64,
}

impl Walked {
	/// The manifest form of every entry, in walk order.
	pub fn entries(&self) -> Vec<ManifestEntry> {
		self.items.iter().map(|w| w.entry.clone()).collect()
	}

	/// Just the file entries (skipping directories), in stream order.
	pub fn files(&self) -> impl Iterator<Item = &WalkItem> {
		self.items.iter().filter(|w| !w.entry.is_dir)
	}
}

/// The transfer-root-relative path for a file: `root_name` combined with the entry's
/// `rel`. A single-file transfer has an empty `rel`, so this is just the root name.
/// Both ends build the receiver's target as `safe_join(dest_dir, under_root(..))`.
pub fn under_root(root_name: &str, rel: &str) -> String {
	if rel.is_empty() {
		root_name.to_string()
	} else {
		format!("{root_name}/{rel}")
	}
}

/// A path relative to `root`, as forward-slash components, so it's portable over the
/// wire regardless of the source OS separator.
fn rel_path(root: &Path, path: &Path) -> String {
	match path.strip_prefix(root) {
		Ok(rel) => rel
			.components()
			.filter_map(|c| match c {
				Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
				_ => None,
			})
			.collect::<Vec<_>>()
			.join("/"),
		Err(_) => path
			.file_name()
			.map(|s| s.to_string_lossy().into_owned())
			.unwrap_or_default(),
	}
}

/// Enumerate a source path into a flat manifest. A single file becomes one entry with
/// an empty `rel`; a directory is walked depth-first, emitting an entry for every
/// subdirectory (so empty ones round-trip) and every file. Symlinks are skipped to
/// avoid loops; unreadable subdirectories are skipped rather than aborting the walk.
/// The blocking `std::fs` walk should be run on a blocking task by the caller.
pub fn enumerate(root: &Path) -> std::io::Result<Walked> {
	let meta = std::fs::symlink_metadata(root)?;
	let root_name = root
		.file_name()
		.map(|s| s.to_string_lossy().into_owned())
		.unwrap_or_else(|| root.to_string_lossy().into_owned());

	if meta.is_file() {
		let size = meta.len();
		return Ok(Walked {
			root_name,
			root_is_dir: false,
			total_bytes: size,
			items: vec![WalkItem {
				entry: ManifestEntry {
					rel: String::new(),
					size,
					mtime: mtime_secs(&meta),
					is_dir: false,
				},
				abs: root.to_path_buf(),
			}],
		});
	}
	if !meta.is_dir() {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidInput,
			format!("{}: not a regular file or directory", root.display()),
		));
	}

	let mut items = Vec::new();
	let mut total_bytes = 0u64;
	let mut stack = vec![root.to_path_buf()];
	while let Some(dir) = stack.pop() {
		let Ok(rd) = std::fs::read_dir(&dir) else { continue };
		for entry in rd.flatten() {
			let path = entry.path();
			let Ok(ft) = entry.file_type() else { continue };
			let rel = rel_path(root, &path);
			if ft.is_symlink() {
				continue;
			} else if ft.is_dir() {
				items.push(WalkItem {
					entry: ManifestEntry {
						rel,
						size: 0,
						mtime: None,
						is_dir: true,
					},
					abs: path.clone(),
				});
				stack.push(path);
			} else if ft.is_file() {
				let m = entry.metadata().ok();
				let size = m.as_ref().map(|m| m.len()).unwrap_or(0);
				let mtime = m.as_ref().and_then(mtime_secs);
				total_bytes += size;
				items.push(WalkItem {
					entry: ManifestEntry {
						rel,
						size,
						mtime,
						is_dir: false,
					},
					abs: path,
				});
			}
		}
	}
	Ok(Walked {
		root_name,
		root_is_dir: true,
		items,
		total_bytes,
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn chunks_round_trip_and_eof_is_empty() {
		let (mut a, mut b) = tokio::io::duplex(1 << 16);
		write_chunk(&mut a, b"hello").await.unwrap();
		write_chunk(&mut a, b"world").await.unwrap();
		write_eof(&mut a).await.unwrap();

		assert_eq!(read_chunk(&mut b).await.unwrap(), b"hello");
		assert_eq!(read_chunk(&mut b).await.unwrap(), b"world");
		assert!(
			read_chunk(&mut b).await.unwrap().is_empty(),
			"eof is a zero-length chunk"
		);
	}

	#[test]
	fn manifest_chunk_is_tagged() {
		let json = serde_json::to_string(&ManifestChunk::End).unwrap();
		assert_eq!(json, r#"{"t":"end"}"#);
		let json = serde_json::to_string(&ManifestChunk::Entries(vec![])).unwrap();
		assert!(json.contains(r#""t":"entries""#));
	}

	#[test]
	fn safe_join_rejects_escapes_and_accepts_plain_paths() {
		let base = Path::new("/srv/dest");
		assert_eq!(safe_join(base, "a/b.txt"), Some(PathBuf::from("/srv/dest/a/b.txt")));
		assert_eq!(safe_join(base, "./a//b"), Some(PathBuf::from("/srv/dest/a/b")));
		assert_eq!(safe_join(base, ""), Some(base.to_path_buf()));
		assert!(safe_join(base, "../etc/passwd").is_none());
		assert!(safe_join(base, "a/../../x").is_none());
	}

	fn tmp(name: &str) -> PathBuf {
		use std::sync::atomic::{AtomicU32, Ordering};
		static N: AtomicU32 = AtomicU32::new(0);
		let p = std::env::temp_dir().join(format!(
			"lt-xfer-{}-{}-{name}",
			std::process::id(),
			N.fetch_add(1, Ordering::Relaxed)
		));
		let _ = std::fs::remove_dir_all(&p);
		std::fs::create_dir_all(&p).unwrap();
		p
	}

	/// A full sender↔receiver round trip over a duplex pair, then a resumed run that
	/// only needs to ship the remaining bytes.
	#[tokio::test]
	async fn send_receive_round_trips_and_resumes() {
		let dir = tmp("rt");
		let src = dir.join("src.bin");
		let dst_dir = dir.join("out");
		std::fs::create_dir_all(&dst_dir).unwrap();
		let target = dst_dir.join("copy.bin");
		let data: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
		std::fs::write(&src, &data).unwrap();
		let header = FileHeader {
			index: 0,
			rel: "copy.bin".into(),
			size: data.len() as u64,
			mtime: Some(1_700_000_000),
		};

		// Fresh transfer: receiver has nothing, gets the whole file.
		{
			let (mut cs, mut sr) = tokio::io::duplex(1 << 16); // sender.send -> receiver.recv
			let (mut ss, mut cr) = tokio::io::duplex(1 << 16); // receiver.send -> sender.recv
			let src2 = src.clone();
			let sz = header.size;
			let sender = tokio::spawn(async move { send_file(&mut cs, &mut cr, &src2, sz, |_| {}).await });
			let got = receive_file(&mut ss, &mut sr, &target, &header, |_| {}).await.unwrap();
			sender.await.unwrap().unwrap();
			assert_eq!(got, data.len() as u64);
			assert_eq!(std::fs::read(&target).unwrap(), data);
		}

		// Simulate an interrupted resume: drop the final, stage a partial + matching
		// meta, and confirm the transfer completes from the partial.
		{
			std::fs::remove_file(&target).unwrap();
			std::fs::write(part_path(&target), &data[..100_000]).unwrap();
			std::fs::write(
				meta_path(&target),
				serde_json::to_vec(&PartMeta {
					size: header.size,
					mtime: header.mtime,
				})
				.unwrap(),
			)
			.unwrap();

			let (mut cs, mut sr) = tokio::io::duplex(1 << 16);
			let (mut ss, mut cr) = tokio::io::duplex(1 << 16);
			let src2 = src.clone();
			let sz = header.size;
			let sender = tokio::spawn(async move { send_file(&mut cs, &mut cr, &src2, sz, |_| {}).await });
			let got = receive_file(&mut ss, &mut sr, &target, &header, |_| {}).await.unwrap();
			sender.await.unwrap().unwrap();
			assert_eq!(got, data.len() as u64);
			assert_eq!(std::fs::read(&target).unwrap(), data);
			assert!(!part_path(&target).exists(), "partial is renamed away on completion");
		}

		let _ = std::fs::remove_dir_all(&dir);
	}

	/// When the receiver already holds the exact file, the sender ships only the EOF
	/// marker and nothing is rewritten.
	#[tokio::test]
	async fn already_complete_file_is_skipped() {
		let dir = tmp("skip");
		let src = dir.join("s.bin");
		let target = dir.join("t.bin");
		let data = vec![7u8; 50_000];
		std::fs::write(&src, &data).unwrap();
		std::fs::write(&target, &data).unwrap();
		let m = std::fs::metadata(&target).unwrap();
		let header = FileHeader {
			index: 0,
			rel: "t.bin".into(),
			size: data.len() as u64,
			mtime: mtime_secs(&m),
		};

		let (mut cs, mut sr) = tokio::io::duplex(1 << 16);
		let (mut ss, mut cr) = tokio::io::duplex(1 << 16);
		let src2 = src.clone();
		let sz = header.size;
		let sender = tokio::spawn(async move { send_file(&mut cs, &mut cr, &src2, sz, |_| {}).await });
		let got = receive_file(&mut ss, &mut sr, &target, &header, |_| {}).await.unwrap();
		sender.await.unwrap().unwrap();
		assert_eq!(got, data.len() as u64);
		assert!(!part_path(&target).exists());
		let _ = std::fs::remove_dir_all(&dir);
	}

	/// A complete, matching final file with a *stale* `.part` beside it must be left
	/// intact (the stale partial is cleaned up, never renamed over the good file).
	#[tokio::test]
	async fn complete_final_with_stale_part_is_not_overwritten() {
		let dir = tmp("stale");
		let src = dir.join("s.bin");
		let target = dir.join("t.bin");
		let good = vec![1u8; 40_000];
		std::fs::write(&src, &good).unwrap();
		std::fs::write(&target, &good).unwrap();
		let m = std::fs::metadata(&target).unwrap();
		let header = FileHeader {
			index: 0,
			rel: "t.bin".into(),
			size: good.len() as u64,
			mtime: mtime_secs(&m),
		};
		// A stale partial (garbage) + a matching meta linger next to the good final.
		std::fs::write(part_path(&target), vec![9u8; 5]).unwrap();
		std::fs::write(
			meta_path(&target),
			serde_json::to_vec(&PartMeta {
				size: header.size,
				mtime: header.mtime,
			})
			.unwrap(),
		)
		.unwrap();

		let (mut cs, mut sr) = tokio::io::duplex(1 << 16);
		let (mut ss, mut cr) = tokio::io::duplex(1 << 16);
		let src2 = src.clone();
		let sz = header.size;
		let sender = tokio::spawn(async move { send_file(&mut cs, &mut cr, &src2, sz, |_| {}).await });
		let got = receive_file(&mut ss, &mut sr, &target, &header, |_| {}).await.unwrap();
		sender.await.unwrap().unwrap();
		assert_eq!(got, good.len() as u64);
		assert_eq!(
			std::fs::read(&target).unwrap(),
			good,
			"the good final file must be untouched"
		);
		assert!(!part_path(&target).exists(), "the stale partial is cleaned up");
		let _ = std::fs::remove_dir_all(&dir);
	}
}
