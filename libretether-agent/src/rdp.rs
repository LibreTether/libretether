//! Enable an RDP server on the client so the controller can attach a standard
//! RDP viewer over the tailnet.
//!
//! On Linux this drives **gnome-remote-desktop** via `grdctl` — which manages
//! its own credentials, so there's no per-connect Wayland portal prompt. On
//! Windows it enables the built-in Remote Desktop service (using the machine's
//! existing account credentials). macOS has no built-in RDP server.

#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::process::Command;

use libretether_protocol::RdpInfo;

#[cfg(any(target_os = "linux", target_os = "windows"))]
const RDP_PORT: u16 = 3389;

/// Enable RDP and return how to reach it. Blocking — call via `spawn_blocking`.
pub fn enable() -> Result<RdpInfo, String> {
	#[cfg(target_os = "linux")]
	{
		enable_linux()
	}
	#[cfg(target_os = "windows")]
	{
		enable_windows()
	}
	#[cfg(target_os = "macos")]
	{
		Err("RDP isn't supported on macOS (no built-in RDP server) — use the live screen control instead.".to_string())
	}
}

#[cfg(target_os = "linux")]
fn enable_linux() -> Result<RdpInfo, String> {
	use libretether_protocol::crypto::random_alnum;

	if !libretether_common::which("grdctl") {
		return Err(
			"gnome-remote-desktop (grdctl) not found — install it on the client, e.g. `apt install gnome-remote-desktop`."
				.to_string(),
		);
	}

	let username = "libretether".to_string();
	let password = random_alnum(16);

	ensure_tls_cert();
	set_credentials(&username, &password)?;
	let _ = grd(&["rdp", "disable-view-only"]); // allow control, not view-only
	grd(&["rdp", "enable"])?;
	let _ = Command::new("systemctl")
		.args(["--user", "enable", "--now", "gnome-remote-desktop.service"])
		.status();

	Ok(RdpInfo {
		backend: "gnome-remote-desktop".to_string(),
		address: crate::host::tailscale_ip(),
		port: RDP_PORT,
		username,
		password: Some(password),
		note: Some("Needs a logged-in graphical session; gnome-remote-desktop pauses at the lock screen.".to_string()),
	})
}

/// Set the gnome-remote-desktop RDP credentials. The password is fed on stdin so
/// it never appears in the process argument list (which is world-readable via
/// `/proc/<pid>/cmdline`).
///
/// There is deliberately **no argv fallback**: passing the password as a command
/// line argument would leak it to any local process, defeating the protection. A
/// grdctl too old to read it on stdin fails closed with an upgrade message.
#[cfg(target_os = "linux")]
fn set_credentials(username: &str, password: &str) -> Result<(), String> {
	use std::io::Write;
	use std::process::Stdio;

	let mut child = Command::new("grdctl")
		.args(["rdp", "set-credentials", username])
		.stdin(Stdio::piped())
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.map_err(|e| format!("spawning grdctl: {e}"))?;
	child
		.stdin
		.take()
		.ok_or_else(|| "grdctl stdin was unavailable".to_string())?
		.write_all(format!("{password}\n").as_bytes())
		.map_err(|e| format!("writing the RDP password to grdctl: {e}"))?;
	if child.wait().map_err(|e| format!("waiting for grdctl: {e}"))?.success() {
		Ok(())
	} else {
		Err(
			"grdctl rejected the RDP credentials on stdin — upgrade gnome-remote-desktop \
		     (the password is never passed on the command line)"
				.to_string(),
		)
	}
}

#[cfg(target_os = "linux")]
fn grd(args: &[&str]) -> Result<(), String> {
	let out = Command::new("grdctl")
		.args(args)
		.output()
		.map_err(|e| format!("grdctl {args:?}: {e}"))?;
	if out.status.success() {
		Ok(())
	} else {
		Err(format!(
			"grdctl {args:?} failed: {}",
			String::from_utf8_lossy(&out.stderr).trim()
		))
	}
}

/// Ensure gnome-remote-desktop's RDP has a TLS cert (older GNOME won't start
/// RDP without one). Best-effort self-signed cert via openssl.
#[cfg(target_os = "linux")]
fn ensure_tls_cert() {
	let Some(dir) = dirs::data_dir().map(|d| d.join("libretether-agent")) else {
		return;
	};
	let cert = dir.join("rdp-cert.pem");
	let key = dir.join("rdp-key.pem");
	if !cert.exists() || !key.exists() {
		let _ = std::fs::create_dir_all(&dir);
		let made = Command::new("openssl")
			.args([
				"req",
				"-x509",
				"-newkey",
				"rsa:2048",
				"-nodes",
				"-days",
				"3650",
				"-subj",
				"/CN=libretether-agent",
				"-keyout",
				&key.to_string_lossy(),
				"-out",
				&cert.to_string_lossy(),
			])
			.status()
			.map(|s| s.success())
			.unwrap_or(false);
		if !made {
			return;
		}
	}
	let _ = grd(&["rdp", "set-tls-cert", &cert.to_string_lossy()]);
	let _ = grd(&["rdp", "set-tls-key", &key.to_string_lossy()]);
}

#[cfg(target_os = "windows")]
const TS_KEY: &str = r"System\CurrentControlSet\Control\Terminal Server";

#[cfg(target_os = "windows")]
fn enable_windows() -> Result<RdpInfo, String> {
	ensure_rdp_host_enabled()?;

	// Bring up the Remote Desktop Services listener. Starting a system service needs
	// admin — which the agent usually lacks — so this is best-effort; the verify step
	// below turns "enabled but nothing listening" into a clear error instead of a
	// false success.
	let _ = run_ps("Start-Service -Name TermService");

	// Best-effort: open the Remote Desktop firewall group. Use the canonical,
	// locale-independent group id — the display name "Remote Desktop" is translated
	// (e.g. "Área de Trabalho Remota" on pt-BR Windows) so matching by DisplayGroup
	// silently misses on a localized install. This is also admin-only and may already
	// be open; a still-blocked firewall surfaces as a refused connection the operator
	// can fix, not a wrong state we should fail on here.
	let _ = run_ps("Enable-NetFirewallRule -Group '@FirewallAPI.dll,-28752'");

	// `fDenyTSConnections = 0` does NOT guarantee an RDP server is actually listening:
	// Windows Home has no RDP host at all, and on Pro the listener may not be bound
	// until TermService (re)starts. The controller then tunnels to this machine's own
	// 127.0.0.1:3389 — loopback, so not firewall-gated — and a missing listener shows
	// up as a bare "connection refused" (os error 10061) in the RDP viewer, long after
	// we'd already claimed success. Verify the port accepts before reporting enabled,
	// and otherwise fail closed with an actionable, edition-aware reason.
	if !rdp_port_listening() {
		return Err(rdp_unavailable_reason());
	}

	Ok(RdpInfo {
		backend: "windows".to_string(),
		address: crate::host::tailscale_ip(),
		port: RDP_PORT,
		username: whoami::username(),
		password: None, // use the machine's existing Windows account password
		note: Some(
			"Sign in with this PC's Windows account password when prompted (RDP needs Windows Pro+).".to_string(),
		),
	})
}

/// Does something accept TCP on the local RDP port yet? The RDP-Tcp listener can take
/// a moment to bind after TermService starts, so retry briefly (a *refused* connect
/// returns immediately, so this mostly costs the sleeps, ~3s worst case) before
/// concluding there's no RDP server.
#[cfg(target_os = "windows")]
fn rdp_port_listening() -> bool {
	use std::net::{Ipv4Addr, SocketAddr, TcpStream};
	use std::time::Duration;

	let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, RDP_PORT));
	for attempt in 0..6 {
		if TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
			return true;
		}
		if attempt < 5 {
			std::thread::sleep(Duration::from_millis(500));
		}
	}
	false
}

/// Why isn't the RDP host listening? Windows Home simply has no Remote Desktop server,
/// which we can tell apart from a stopped-service situation via the edition id (Home
/// SKUs are the "Core*" family) so the operator gets the right next step.
#[cfg(target_os = "windows")]
fn rdp_unavailable_reason() -> String {
	use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};
	use winreg::RegKey;

	let is_home = RegKey::predef(HKEY_LOCAL_MACHINE)
		.open_subkey_with_flags(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", KEY_READ)
		.and_then(|k| k.get_value::<String, _>("EditionID"))
		.map(|e| e.to_ascii_lowercase().starts_with("core"))
		.unwrap_or(false);

	if is_home {
		"Remote Desktop was enabled, but this Windows edition (Home) has no RDP host — nothing \
		 listens on port 3389, so the connection is refused. Use LibreTether's live screen \
		 Control/Watch instead, or upgrade the guest to Windows Pro."
			.to_string()
	} else {
		"Remote Desktop is enabled but nothing is listening on port 3389 (connection refused). \
		 The Remote Desktop Services (TermService) couldn't be started — the agent runs \
		 unprivileged and can't start a system service. Re-run the installer as administrator, \
		 start 'Remote Desktop Services' on the PC, or reboot it. Live screen Control/Watch \
		 works without RDP."
			.to_string()
	}
}

/// Clear `fDenyTSConnections` so the built-in RDP host accepts connections.
///
/// The value lives under HKLM, so only Administrators may write it. The agent runs
/// unprivileged by design (per-user autostart, no SYSTEM service — see `service.rs`),
/// so the installer is the intended place this gets turned on: it self-elevates that
/// one step under a one-time UAC prompt. Reading the value is fine unprivileged, so
/// this is idempotent — when it's already cleared (the healthy, installer-enabled
/// case) we never touch it and never need elevation.
///
/// If it *isn't* enabled and we can't write it, we fail closed with an actionable
/// message (re-run the installer as admin / turn it on in Settings) rather than
/// surfacing a raw, localized registry `SecurityException` to the operator.
#[cfg(target_os = "windows")]
fn ensure_rdp_host_enabled() -> Result<(), String> {
	use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE};
	use winreg::RegKey;

	let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);

	// Already enabled? A non-admin can read HKLM but can't open it for write, so we
	// must check this first — otherwise a guest the installer already enabled would
	// hit an access-denied write and error out despite RDP being on.
	if let Ok(key) = hklm.open_subkey_with_flags(TS_KEY, KEY_READ) {
		if key
			.get_value::<u32, _>("fDenyTSConnections")
			.map(|v| v == 0)
			.unwrap_or(false)
		{
			return Ok(());
		}
	}

	hklm.open_subkey_with_flags(TS_KEY, KEY_SET_VALUE)
		.and_then(|key| key.set_value("fDenyTSConnections", &0u32))
		.map_err(|_| {
			"enabling the Remote Desktop host needs administrator rights, but the LibreTether \
			 agent runs unprivileged. On this PC, re-run the installer as administrator (or turn \
			 on Settings → System → Remote Desktop), then retry. Live screen Control/Watch works \
			 without RDP."
				.to_string()
		})
}

#[cfg(target_os = "windows")]
fn run_ps(script: &str) -> Result<(), String> {
	use crate::proc::NoWindow;

	let out = Command::new("powershell")
		.args(["-NoProfile", "-NonInteractive", "-Command", script])
		.no_window()
		.output()
		.map_err(|e| format!("powershell: {e}"))?;
	if out.status.success() {
		Ok(())
	} else {
		Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
	}
}
