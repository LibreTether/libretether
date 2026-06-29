//! Writing secret-bearing files (identity seeds, enrollment tokens, relay
//! secrets, TLS keys) so other local users can't read them.
//!
//! Writes are **atomic and crash-safe**: contents go to a temp file in the same
//! directory (so it shares a filesystem with the target), are flushed to disk,
//! and then `rename`d over the destination — a power loss or panic mid-write
//! leaves either the old file or the new one intact, never a truncated mix.
//! This matters because these files hold the controller's client registry and
//! enrollment tokens; a torn write there would force every agent to re-enroll.
//!
//! On Unix the temp file is created with mode `0600` and its parent directory
//! tightened to `0700`. On other platforms it falls back to a plain atomic
//! replace (the file inherits the user-profile ACL).

use std::io;
use std::path::{Path, PathBuf};

/// Atomically write `contents` to `path`, creating parent directories, with
/// owner-only permissions on Unix.
///
/// The write goes to a sibling temp file that is fsync'd and then renamed over
/// `path`, so a concurrent reader (or a crash) never observes a partial file.
pub fn write(path: impl AsRef<Path>, contents: &[u8]) -> io::Result<()> {
	let path = path.as_ref();
	if let Some(dir) = path.parent() {
		if !dir.as_os_str().is_empty() {
			std::fs::create_dir_all(dir)?;
			#[cfg(unix)]
			{
				use std::os::unix::fs::PermissionsExt;
				// Best-effort: tighten the immediate parent. Intermediate dirs
				// keep their existing mode (the 0600 file is the real guard).
				let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
			}
		}
	}

	let tmp = temp_sibling(path);
	// Clean up the temp file on any early-return error so a failed write doesn't
	// litter a `.tmp` next to the target. Success renames it away first.
	let result = write_then_rename(&tmp, path, contents);
	if result.is_err() {
		let _ = std::fs::remove_file(&tmp);
	}
	result
}

/// Write `contents` to `tmp`, fsync it, then atomically rename it over `path`.
fn write_then_rename(tmp: &Path, path: &Path, contents: &[u8]) -> io::Result<()> {
	use std::io::Write;
	{
		#[cfg(unix)]
		let mut f = {
			use std::os::unix::fs::OpenOptionsExt;
			std::fs::OpenOptions::new()
				.write(true)
				.create(true)
				.truncate(true)
				.mode(0o600)
				.open(tmp)?
		};
		#[cfg(not(unix))]
		let mut f = std::fs::File::create(tmp)?;

		f.write_all(contents)?;
		// fsync the data before the rename so a crash can't leave the renamed
		// file present but empty (rename is ordered after the data hits disk).
		f.sync_all()?;
	}
	// `rename` is atomic on a single filesystem, which the sibling temp guarantees.
	std::fs::rename(tmp, path)
}

/// A temp path next to `path` (same directory → same filesystem, so the final
/// `rename` is atomic). Includes the pid so two processes don't collide.
fn temp_sibling(path: &Path) -> PathBuf {
	let pid = std::process::id();
	let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("secret");
	let tmp_name = format!(".{name}.{pid}.tmp");
	match path.parent() {
		Some(dir) if !dir.as_os_str().is_empty() => dir.join(tmp_name),
		_ => PathBuf::from(tmp_name),
	}
}

/// Convenience wrapper for writing a string.
pub fn write_str(path: impl AsRef<Path>, contents: &str) -> io::Result<()> {
	write(path, contents.as_bytes())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn temp_path(tag: &str) -> PathBuf {
		use std::sync::atomic::{AtomicU32, Ordering};
		static N: AtomicU32 = AtomicU32::new(0);
		std::env::temp_dir().join(format!(
			"lt-secret-{}-{}-{}",
			std::process::id(),
			tag,
			N.fetch_add(1, Ordering::Relaxed)
		))
	}

	#[test]
	fn write_then_read_round_trips_and_creates_parent_dirs() {
		let dir = temp_path("dir");
		let path = dir.join("nested").join("config.json");
		write_str(&path, "hello").unwrap();
		assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn overwrite_replaces_contents_and_leaves_no_temp_file() {
		let path = temp_path("overwrite");
		write_str(&path, "first").unwrap();
		write_str(&path, "second-longer").unwrap();
		assert_eq!(std::fs::read_to_string(&path).unwrap(), "second-longer");
		// The atomic temp sibling must be gone after a successful write.
		let tmp = temp_sibling(&path);
		assert!(!tmp.exists(), "temp sibling {tmp:?} should be cleaned up");
		let _ = std::fs::remove_file(&path);
	}

	// The rename is atomic: a reader either sees the whole old file or the whole
	// new one. We can't crash mid-write in a unit test, but we can prove the
	// destination is never opened for truncation in place — overwriting a file
	// while a reader holds an open handle to the *old* inode still yields the old
	// contents (the new data went to a different inode).
	#[test]
	fn overwrite_does_not_truncate_the_existing_inode() {
		use std::io::Read;
		let path = temp_path("inode");
		write_str(&path, "OLD-CONTENTS").unwrap();
		let mut reader = std::fs::File::open(&path).unwrap();
		// Replace while the old handle is open.
		write_str(&path, "NEW").unwrap();
		let mut buf = String::new();
		reader.read_to_string(&mut buf).unwrap();
		assert_eq!(
			buf, "OLD-CONTENTS",
			"the old inode must be intact (not truncated in place)"
		);
		assert_eq!(std::fs::read_to_string(&path).unwrap(), "NEW");
		let _ = std::fs::remove_file(&path);
	}

	#[cfg(unix)]
	#[test]
	fn writes_owner_only_permissions() {
		use std::os::unix::fs::PermissionsExt;
		let path = temp_path("perms");
		write_str(&path, "secret").unwrap();
		let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
		assert_eq!(mode, 0o600, "secret files must be owner-only");
		let _ = std::fs::remove_file(&path);
	}

	#[cfg(unix)]
	#[test]
	fn overwriting_a_loose_mode_file_tightens_it() {
		use std::os::unix::fs::PermissionsExt;
		let path = temp_path("tighten");
		std::fs::write(&path, "world-readable").unwrap();
		std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
		// An atomic replace swaps in a fresh 0600 inode regardless of the old mode.
		write_str(&path, "now-secret").unwrap();
		let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
		assert_eq!(mode, 0o600);
		let _ = std::fs::remove_file(&path);
	}
}
