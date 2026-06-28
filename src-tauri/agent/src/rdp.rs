//! Enable an RDP server on the client so the controller can attach a standard
//! RDP viewer over the tailnet.
//!
//! On Linux this drives **gnome-remote-desktop** via `grdctl` — which manages
//! its own credentials, so there's no per-connect Wayland portal prompt. On
//! Windows it enables the built-in Remote Desktop service (using the machine's
//! existing account credentials). macOS has no built-in RDP server.

#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::process::Command;

use tether_protocol::RdpInfo;

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
fn has(bin: &str) -> bool {
	Command::new(bin).arg("--help").output().is_ok()
}

#[cfg(target_os = "linux")]
fn enable_linux() -> Result<RdpInfo, String> {
	use tether_protocol::crypto::random_alnum;

	if !has("grdctl") {
		return Err(
			"gnome-remote-desktop (grdctl) not found — install it on the client, e.g. `apt install gnome-remote-desktop`."
				.to_string(),
		);
	}

	let username = "tether".to_string();
	let password = random_alnum(16);

	ensure_tls_cert();
	grd(&["rdp", "set-credentials", &username, &password])?;
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
	let Some(dir) = dirs::data_dir().map(|d| d.join("tether-agent")) else {
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
				"/CN=tether-agent",
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
fn enable_windows() -> Result<RdpInfo, String> {
	run_ps(
		"Set-ItemProperty -Path 'HKLM:\\System\\CurrentControlSet\\Control\\Terminal Server' -Name 'fDenyTSConnections' -Value 0",
	)?;
	let _ = run_ps("Enable-NetFirewallRule -DisplayGroup 'Remote Desktop'");

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

#[cfg(target_os = "windows")]
fn run_ps(script: &str) -> Result<(), String> {
	let out = Command::new("powershell")
		.args(["-NoProfile", "-NonInteractive", "-Command", script])
		.output()
		.map_err(|e| format!("powershell: {e}"))?;
	if out.status.success() {
		Ok(())
	} else {
		Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
	}
}
