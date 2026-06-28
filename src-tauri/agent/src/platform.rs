//! Runtime display-server detection.

/// True when running under a Wayland session.
///
/// We treat the presence of `WAYLAND_DISPLAY` (or `XDG_SESSION_TYPE=wayland`) as
/// authoritative — on GNOME Wayland an XWayland fallback exists but can't
/// capture or control native Wayland windows, so we prefer the portal path.
#[cfg_attr(not(feature = "wayland"), allow(dead_code))]
pub fn is_wayland() -> bool {
	if std::env::var_os("WAYLAND_DISPLAY").is_some() {
		return true;
	}
	matches!(std::env::var("XDG_SESSION_TYPE").as_deref(), Ok("wayland"))
}
