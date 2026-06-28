//! Launch the host's RDP client to connect to a client over the tailnet.
//! We shell out to the system RDP viewer (FreeRDP / mstsc) rather than embed
//! one — the connection rides Tailscale straight to the client's private IP.

use std::process::Command;

use crate::error::{AppError, AppResult};

/// Launch a local RDP viewer connecting to `host:port`.
pub fn launch(host: &str, port: u16, username: &str, password: Option<&str>) -> AppResult<()> {
	#[cfg(target_os = "linux")]
	{
		launch_linux(host, port, username, password)
	}
	#[cfg(target_os = "windows")]
	{
		launch_windows(host, port, username, password)
	}
	#[cfg(target_os = "macos")]
	{
		launch_macos(host, port, username)
	}
}

#[cfg(target_os = "linux")]
fn launch_linux(host: &str, port: u16, username: &str, password: Option<&str>) -> AppResult<()> {
	let bin = ["xfreerdp3", "xfreerdp", "wlfreerdp"]
		.into_iter()
		.find(|b| which(b))
		.ok_or_else(|| AppError::msg("No RDP client found — install FreeRDP, e.g. `apt install freerdp3-x11`."))?;

	let mut cmd = Command::new(bin);
	cmd.arg(format!("/v:{host}:{port}"));
	cmd.arg(format!("/u:{username}"));
	if let Some(pw) = password {
		cmd.arg(format!("/p:{pw}"));
	}
	cmd.args(["/cert:ignore", "/dynamic-resolution", "+clipboard"]);
	cmd.spawn()
		.map_err(|e| AppError::msg(format!("launching {bin}: {e}")))?;
	Ok(())
}

#[cfg(target_os = "linux")]
fn which(bin: &str) -> bool {
	Command::new("which")
		.arg(bin)
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn launch_windows(host: &str, port: u16, username: &str, password: Option<&str>) -> AppResult<()> {
	// Stash credentials so mstsc doesn't prompt (only when we manage them).
	if let Some(pw) = password {
		let _ = Command::new("cmdkey")
			.arg(format!("/generic:TERMSRV/{host}"))
			.arg(format!("/user:{username}"))
			.arg(format!("/pass:{pw}"))
			.status();
	}
	Command::new("mstsc")
		.arg(format!("/v:{host}:{port}"))
		.spawn()
		.map_err(|e| AppError::msg(format!("launching mstsc: {e}")))?;
	Ok(())
}

#[cfg(target_os = "macos")]
fn launch_macos(host: &str, port: u16, username: &str) -> AppResult<()> {
	// Microsoft Remote Desktop URL scheme (it will prompt for the password).
	let url = format!("rdp://full%20address=s:{host}:{port}&username=s:{username}");
	Command::new("open")
		.arg(url)
		.spawn()
		.map_err(|e| AppError::msg(format!("opening RDP url: {e}")))?;
	Ok(())
}
