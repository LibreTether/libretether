//! Launch the host's RDP client to connect to a client over the tailnet.
//! We shell out to a system RDP viewer (FreeRDP / Remmina / GNOME Connections /
//! mstsc) rather than embed one — the connection rides Tailscale straight to the
//! client's private IP. The viewer is the user's choice (controller setting).

use std::process::Command;

use crate::error::{AppError, AppResult};

/// Launch an RDP viewer connecting to `host:port`. `pref` is the controller's
/// preferred client ("auto"/"freerdp"/"remmina"/"gnome-connections" or a custom
/// command template); ignored on platforms with a single obvious client.
pub fn launch(pref: Option<&str>, host: &str, port: u16, username: &str, password: Option<&str>) -> AppResult<()> {
	#[cfg(target_os = "linux")]
	{
		launch_linux(pref.unwrap_or("auto").trim(), host, port, username, password)
	}
	#[cfg(target_os = "windows")]
	{
		let _ = pref;
		launch_windows(host, port, username, password)
	}
	#[cfg(target_os = "macos")]
	{
		// macOS launches via the `open rdp://…` URL scheme, which takes no
		// password (entered interactively / via Keychain).
		let _ = (pref, password);
		launch_macos(host, port, username)
	}
}

#[cfg(target_os = "linux")]
fn launch_linux(pref: &str, host: &str, port: u16, username: &str, password: Option<&str>) -> AppResult<()> {
	match pref {
		"" | "auto" => freerdp(host, port, username, password)
			.or_else(|_| remmina(host, port, username, password))
			.or_else(|_| gnome_connections(host, port))
			.map_err(|_| AppError::msg("No RDP client found — install FreeRDP, Remmina, or GNOME Connections.")),
		"freerdp" => freerdp(host, port, username, password),
		"remmina" => remmina(host, port, username, password),
		"gnome-connections" | "gnome" => gnome_connections(host, port),
		// Anything else is treated as a custom command template.
		template => custom(template, host, port, username, password),
	}
}

#[cfg(target_os = "linux")]
fn freerdp(host: &str, port: u16, username: &str, password: Option<&str>) -> AppResult<()> {
	let bin = ["xfreerdp3", "xfreerdp", "wlfreerdp"]
		.into_iter()
		.find(|b| libretether_common::which(b))
		.ok_or_else(|| AppError::msg("FreeRDP not found (install `freerdp3-x11`)."))?;
	let mut cmd = Command::new(bin);
	cmd.arg(format!("/v:{host}:{port}")).arg(format!("/u:{username}"));
	if let Some(pw) = password {
		cmd.arg(format!("/p:{pw}"));
	}
	cmd.args(["/cert:ignore", "/dynamic-resolution", "+clipboard"]);
	spawn(cmd, bin)
}

#[cfg(target_os = "linux")]
fn remmina(host: &str, port: u16, username: &str, password: Option<&str>) -> AppResult<()> {
	if !libretether_common::which("remmina") {
		return Err(AppError::msg("Remmina not found (install `remmina`)."));
	}
	let url = match password {
		Some(pw) => format!("rdp://{username}:{pw}@{host}:{port}"),
		None => format!("rdp://{username}@{host}:{port}"),
	};
	let mut cmd = Command::new("remmina");
	cmd.arg("-c").arg(url);
	spawn(cmd, "remmina")
}

#[cfg(target_os = "linux")]
fn gnome_connections(host: &str, port: u16) -> AppResult<()> {
	if !libretether_common::which("gnome-connections") {
		return Err(AppError::msg(
			"GNOME Connections not found (install `gnome-connections`).",
		));
	}
	let mut cmd = Command::new("gnome-connections");
	cmd.arg(format!("rdp://{host}:{port}"));
	spawn(cmd, "gnome-connections")
}

/// Run a user-provided command template, substituting placeholders per token.
#[cfg(target_os = "linux")]
fn custom(template: &str, host: &str, port: u16, username: &str, password: Option<&str>) -> AppResult<()> {
	let mut tokens = template.split_whitespace().map(|tok| {
		tok.replace("{host}", host)
			.replace("{address}", host)
			.replace("{port}", &port.to_string())
			.replace("{user}", username)
			.replace("{password}", password.unwrap_or(""))
	});
	let bin = tokens.next().ok_or_else(|| AppError::msg("empty RDP client command"))?;
	let mut cmd = Command::new(&bin);
	cmd.args(tokens);
	spawn(cmd, &bin)
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn spawn(mut cmd: Command, label: &str) -> AppResult<()> {
	cmd.spawn()
		.map(|_| ())
		.map_err(|e| AppError::msg(format!("launching {label}: {e}")))
}

#[cfg(target_os = "windows")]
fn launch_windows(host: &str, port: u16, username: &str, password: Option<&str>) -> AppResult<()> {
	if let Some(pw) = password {
		let _ = Command::new("cmdkey")
			.arg(format!("/generic:TERMSRV/{host}"))
			.arg(format!("/user:{username}"))
			.arg(format!("/pass:{pw}"))
			.status();
	}
	let mut cmd = Command::new("mstsc");
	cmd.arg(format!("/v:{host}:{port}"));
	spawn(cmd, "mstsc")
}

#[cfg(target_os = "macos")]
fn launch_macos(host: &str, port: u16, username: &str) -> AppResult<()> {
	let url = format!("rdp://full%20address=s:{host}:{port}&username=s:{username}");
	let mut cmd = Command::new("open");
	cmd.arg(url);
	spawn(cmd, "open")
}
