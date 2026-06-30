//! Launch a terminal running `ssh` to a client over the tailnet.

use std::path::Path;
use std::process::Command;

use crate::error::{AppError, AppResult};

/// The platform's null device, used as `UserKnownHostsFile` so the agent's
/// ephemeral embedded-server host key is never recorded or checked.
#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";

/// Open a terminal SSH session to `username@host:port`. `terminal_pref` is an
/// optional launcher template like "gnome-terminal --" or "xterm -e"; empty =
/// auto-detect a terminal emulator.
///
/// `identity` is set when connecting to the agent's built-in SSH server: it points
/// at the ephemeral private key, and we then skip host-key verification (the host
/// key is freshly generated and reached over the authenticated tunnel) and offer
/// only that key.
pub fn launch(
	terminal_pref: Option<&str>,
	host: &str,
	port: u16,
	username: &str,
	identity: Option<&Path>,
) -> AppResult<()> {
	crate::logbook::debug("ssh", &format!("spawning ssh client for {username}@{host}:{port}"));
	let mut ssh: Vec<String> = vec!["ssh".into()];
	if let Some(key) = identity {
		ssh.push("-i".into());
		ssh.push(key.to_string_lossy().into_owned());
		ssh.push("-o".into());
		ssh.push("IdentitiesOnly=yes".into());
		ssh.push("-o".into());
		ssh.push("StrictHostKeyChecking=no".into());
		ssh.push("-o".into());
		ssh.push(format!("UserKnownHostsFile={NULL_DEVICE}"));
		ssh.push("-o".into());
		ssh.push("PreferredAuthentications=publickey".into());
	} else {
		ssh.push("-o".into());
		ssh.push("StrictHostKeyChecking=accept-new".into());
	}
	ssh.push("-p".into());
	ssh.push(port.to_string());
	ssh.push(format!("{username}@{host}"));

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
	use crate::launch::{spawn, split_template};

	// A user-set launcher template ("gnome-terminal --", "xterm -e", …).
	if let Some(pref) = pref {
		let (bin, parts) = split_template(pref)?;
		let mut cmd = Command::new(bin);
		cmd.args(parts).args(ssh);
		return spawn(cmd, bin);
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
		if libretether_common::which(bin) {
			let mut cmd = Command::new(bin);
			cmd.args(*prefix).args(ssh);
			return spawn(cmd, bin);
		}
	}
	Err(AppError::msg(
		"No terminal emulator found — set a terminal command in Controller settings.",
	))
}

#[cfg(target_os = "macos")]
fn launch_macos(ssh: &[String]) -> AppResult<()> {
	// `username`/`host` are validated upstream, but escape the command into the
	// AppleScript string literal anyway (backslash + quote) so it can never break
	// out of `do script "…"` into arbitrary AppleScript / shell.
	let escaped = ssh.join(" ").replace('\\', "\\\\").replace('"', "\\\"");
	let script = format!("tell application \"Terminal\" to do script \"{escaped}\"");
	Command::new("osascript")
		.arg("-e")
		.arg(script)
		.spawn()
		.map(|_| ())
		.map_err(|e| AppError::msg(format!("osascript: {e}")))
}

#[cfg(target_os = "windows")]
fn launch_windows(ssh: &[String]) -> AppResult<()> {
	use std::os::windows::process::CommandExt;
	// Launch ssh.exe directly in its own console window (CREATE_NEW_CONSOLE)
	// rather than via `cmd /c start`, which would interpret cmd metacharacters in
	// the destination and allow argument injection.
	const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
	let (bin, args) = ssh.split_first().ok_or_else(|| AppError::msg("empty ssh command"))?;
	Command::new(bin)
		.args(args)
		.creation_flags(CREATE_NEW_CONSOLE)
		.spawn()
		.map(|_| ())
		.map_err(|e| AppError::msg(format!("launching ssh: {e}")))
}
