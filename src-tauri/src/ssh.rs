//! Launch a terminal running `ssh` to a client over the tailnet.

use std::process::Command;

use crate::error::{AppError, AppResult};

/// Open a terminal SSH session to `username@host:port`. `terminal_pref` is an
/// optional launcher template like "gnome-terminal --" or "xterm -e"; empty =
/// auto-detect a terminal emulator.
pub fn launch(terminal_pref: Option<&str>, host: &str, port: u16, username: &str) -> AppResult<()> {
	let ssh: Vec<String> = vec![
		"ssh".into(),
		"-o".into(),
		"StrictHostKeyChecking=accept-new".into(),
		"-p".into(),
		port.to_string(),
		format!("{username}@{host}"),
	];

	#[cfg(target_os = "linux")]
	{
		launch_linux(terminal_pref.map(str::trim).filter(|s| !s.is_empty()), &ssh)
	}
	#[cfg(target_os = "macos")]
	{
		let _ = terminal_pref;
		launch_macos(&ssh)
	}
	#[cfg(target_os = "windows")]
	{
		let _ = terminal_pref;
		launch_windows(&ssh)
	}
}

#[cfg(target_os = "linux")]
fn launch_linux(pref: Option<&str>, ssh: &[String]) -> AppResult<()> {
	// A user-set launcher template ("gnome-terminal --", "xterm -e", …).
	if let Some(pref) = pref {
		let mut parts = pref.split_whitespace();
		let bin = parts.next().ok_or_else(|| AppError::msg("empty terminal command"))?;
		let mut cmd = Command::new(bin);
		cmd.args(parts).args(ssh);
		return cmd
			.spawn()
			.map(|_| ())
			.map_err(|e| AppError::msg(format!("launching {bin}: {e}")));
	}

	// Auto-detect a terminal and the flag it uses to run a command.
	const TERMINALS: &[(&str, &[&str])] = &[
		("x-terminal-emulator", &["-e"]),
		("gnome-terminal", &["--"]),
		("konsole", &["-e"]),
		("xfce4-terminal", &["-e"]),
		("kitty", &[]),
		("foot", &[]),
		("alacritty", &["-e"]),
		("xterm", &["-e"]),
	];
	for (bin, prefix) in TERMINALS {
		if which(bin) {
			let mut cmd = Command::new(bin);
			cmd.args(*prefix).args(ssh);
			return cmd
				.spawn()
				.map(|_| ())
				.map_err(|e| AppError::msg(format!("launching {bin}: {e}")));
		}
	}
	Err(AppError::msg(
		"No terminal emulator found — set a terminal command in Controller settings.",
	))
}

#[cfg(target_os = "linux")]
fn which(bin: &str) -> bool {
	Command::new("which")
		.arg(bin)
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn launch_macos(ssh: &[String]) -> AppResult<()> {
	let script = format!("tell application \"Terminal\" to do script \"{}\"", ssh.join(" "));
	Command::new("osascript")
		.arg("-e")
		.arg(script)
		.spawn()
		.map(|_| ())
		.map_err(|e| AppError::msg(format!("osascript: {e}")))
}

#[cfg(target_os = "windows")]
fn launch_windows(ssh: &[String]) -> AppResult<()> {
	// Open ssh in a new console window.
	let mut cmd = Command::new("cmd");
	cmd.arg("/c").arg("start").arg("").args(ssh);
	cmd.spawn()
		.map(|_| ())
		.map_err(|e| AppError::msg(format!("launching ssh: {e}")))
}
