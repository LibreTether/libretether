//! Writing secret-bearing files (identity seeds, enrollment tokens, relay
//! secrets, TLS keys) so other local users can't read them.
//!
//! On Unix the file is created/truncated with mode `0600` and its parent
//! directory tightened to `0700`. On other platforms it falls back to a plain
//! write (the file inherits the user-profile ACL).

use std::io;
use std::path::Path;

/// Write `contents` to `path`, creating parent directories, with owner-only
/// permissions on Unix.
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

	#[cfg(unix)]
	{
		use std::io::Write;
		use std::os::unix::fs::OpenOptionsExt;
		let mut f = std::fs::OpenOptions::new()
			.write(true)
			.create(true)
			.truncate(true)
			.mode(0o600)
			.open(path)?;
		f.write_all(contents)?;
		f.flush()?;
		// Re-assert the mode in case the file pre-existed with a looser one
		// (OpenOptions::mode only applies when the file is freshly created).
		use std::os::unix::fs::PermissionsExt;
		std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
		Ok(())
	}
	#[cfg(not(unix))]
	{
		std::fs::write(path, contents)
	}
}

/// Convenience wrapper for writing a string.
pub fn write_str(path: impl AsRef<Path>, contents: &str) -> io::Result<()> {
	write(path, contents.as_bytes())
}
