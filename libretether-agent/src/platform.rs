//! Runtime display-server detection.

/// True when running under a Wayland session.
///
/// We treat the presence of `WAYLAND_DISPLAY` (or `XDG_SESSION_TYPE=wayland`) as
/// authoritative — on GNOME Wayland an XWayland fallback exists but can't
/// capture or control native Wayland windows, so we prefer the portal path.
///
/// Those env vars are set in an interactive login but a `systemd --user` service
/// (which we install, and which starts at boot before any graphical login) does
/// **not** inherit them. Relying on them alone makes the agent wrongly take the
/// X11 path and fail with "Authorization required, but no authorization protocol
/// specified". So we also look for the compositor's `wayland-N` socket in
/// `XDG_RUNTIME_DIR`, which the service always has and which only exists when a
/// Wayland compositor is running.
pub fn is_wayland() -> bool {
	if std::env::var_os("WAYLAND_DISPLAY").is_some() {
		return true;
	}
	if matches!(std::env::var("XDG_SESSION_TYPE").as_deref(), Ok("wayland")) {
		return true;
	}
	if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
		if let Ok(entries) = std::fs::read_dir(dir) {
			return entries.flatten().any(|entry| {
				entry
					.file_name()
					.to_str()
					.is_some_and(|name| name.starts_with("wayland-") && !name.ends_with(".lock"))
			});
		}
	}
	false
}
