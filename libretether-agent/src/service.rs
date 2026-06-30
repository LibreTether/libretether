//! Install/uninstall the agent as an always-on background service so the
//! machine stays reachable whenever it's powered on.
//!
//! Screen capture and input injection must run inside the user's graphical
//! session, so on every platform we install a *per-user* autostart (systemd user
//! unit / LaunchAgent / HKCU `Run` entry), not a system-wide daemon — which also
//! means none of it needs elevation.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};

use crate::proc::NoWindow;

fn exe() -> Result<PathBuf> {
	std::env::current_exe().context("locating the agent executable")
}

pub fn install(config_path: &Path) -> Result<()> {
	platform::install(&exe()?, config_path)
}

pub fn uninstall() -> Result<()> {
	platform::uninstall()
}

// Used by the Linux/macOS installers; the Windows path writes the registry + spawns
// directly, so it's dead there.
#[cfg_attr(target_os = "windows", allow(dead_code))]
fn run(cmd: &mut Command) -> Result<()> {
	// Capture output (not inherit) so the underlying tool's own error — e.g. what
	// `schtasks`/`systemctl`/`launchctl` actually complained about — is in the error
	// we return, instead of a bare exit status that tells an operator nothing.
	let output = cmd.no_window().output().with_context(|| format!("running {cmd:?}"))?;
	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr);
		let stdout = String::from_utf8_lossy(&output.stdout);
		let detail = [stderr.trim(), stdout.trim()]
			.into_iter()
			.find(|s| !s.is_empty())
			.unwrap_or("(no output)");
		return Err(anyhow!("command failed ({}): {cmd:?} — {detail}", output.status));
	}
	Ok(())
}

#[cfg(target_os = "linux")]
mod platform {
	use super::*;

	fn unit_path() -> Result<PathBuf> {
		Ok(dirs::config_dir()
			.ok_or_else(|| anyhow!("no config dir"))?
			.join("systemd/user/libretether-agent.service"))
	}

	pub fn install(exe: &Path, config: &Path) -> Result<()> {
		let unit = unit_path()?;
		if let Some(dir) = unit.parent() {
			std::fs::create_dir_all(dir)?;
		}
		let contents = format!(
			"[Unit]\n\
			 Description=LibreTether remote-control agent\n\
			 After=network-online.target graphical-session.target\n\
			 Wants=network-online.target\n\n\
			 [Service]\n\
			 ExecStart=\"{exe}\" run --config \"{config}\"\n\
			 Restart=always\n\
			 RestartSec=3\n\
			 # The session is discovered at runtime: DISPLAY/XAUTHORITY for X11 (from\n\
			 # the live session) and the wayland-N socket in XDG_RUNTIME_DIR for Wayland.\n\n\
			 [Install]\n\
			 WantedBy=default.target\n",
			exe = exe.display(),
			config = config.display(),
		);
		std::fs::write(&unit, contents).with_context(|| format!("writing {}", unit.display()))?;

		run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
		run(Command::new("systemctl").args(["--user", "enable", "libretether-agent.service"]))?;
		// `restart` (not `enable --now`) guarantees a re-deploy picks up the new
		// binary — `--now` leaves an already-running old process in place.
		run(Command::new("systemctl").args(["--user", "restart", "libretether-agent.service"]))?;
		// Keep the service running without an active login session (needs privileges).
		let _ = Command::new("loginctl")
			.args(["enable-linger", &whoami::username()])
			.status();
		println!("Installed and (re)started systemd user service: libretether-agent.service");
		Ok(())
	}

	pub fn uninstall() -> Result<()> {
		let _ = Command::new("systemctl")
			.args(["--user", "disable", "--now", "libretether-agent.service"])
			.status();
		if let Ok(unit) = unit_path() {
			let _ = std::fs::remove_file(unit);
		}
		let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
		println!("Removed systemd user service: libretether-agent.service");
		Ok(())
	}
}

#[cfg(target_os = "macos")]
mod platform {
	use super::*;

	const LABEL: &str = "com.libretether.agent";

	fn plist_path() -> Result<PathBuf> {
		Ok(dirs::home_dir()
			.ok_or_else(|| anyhow!("no home dir"))?
			.join("Library/LaunchAgents")
			.join(format!("{LABEL}.plist")))
	}

	pub fn install(exe: &Path, config: &Path) -> Result<()> {
		let plist = plist_path()?;
		if let Some(dir) = plist.parent() {
			std::fs::create_dir_all(dir)?;
		}
		let contents = format!(
			"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
			 <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
			 <plist version=\"1.0\"><dict>\n\
			 \t<key>Label</key><string>{LABEL}</string>\n\
			 \t<key>ProgramArguments</key><array>\n\
			 \t\t<string>{exe}</string><string>run</string><string>--config</string><string>{config}</string>\n\
			 \t</array>\n\
			 \t<key>RunAtLoad</key><true/>\n\
			 \t<key>KeepAlive</key><true/>\n\
			 </dict></plist>\n",
			exe = exe.display(),
			config = config.display(),
		);
		std::fs::write(&plist, contents).with_context(|| format!("writing {}", plist.display()))?;
		let _ = Command::new("launchctl").arg("unload").arg(&plist).status();
		run(Command::new("launchctl").arg("load").arg(&plist))?;
		println!("Installed and loaded LaunchAgent: {LABEL}");
		println!("Note: grant Screen Recording + Accessibility permissions in System Settings > Privacy.");
		Ok(())
	}

	pub fn uninstall() -> Result<()> {
		if let Ok(plist) = plist_path() {
			let _ = Command::new("launchctl").arg("unload").arg(&plist).status();
			let _ = std::fs::remove_file(plist);
		}
		println!("Removed LaunchAgent: {LABEL}");
		Ok(())
	}
}

#[cfg(target_os = "windows")]
mod platform {
	use std::process::Stdio;

	use winreg::enums::{HKEY_CURRENT_USER, KEY_WRITE};
	use winreg::RegKey;

	use super::*;

	// Autostart via the per-user HKCU Run key, *not* the Task Scheduler. The installer
	// runs non-elevated (a double-clicked .bat / `irm | iex` gets the filtered standard
	// token), and `schtasks /Create` can't write the task store without elevation —
	// it fails with "access denied". The Run key lives in the user's own hive: writable
	// without elevation, and it launches the agent at logon in the interactive session,
	// which is exactly what screen capture and input injection need.
	const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
	const VALUE: &str = "LibreTetherAgent";
	// Legacy: older installs registered this scheduled task. Clean it up on (un)install.
	const LEGACY_TASK: &str = "LibreTetherAgent";

	pub fn install(exe: &Path, config: &Path) -> Result<()> {
		let command = format!("\"{}\" run --config \"{}\"", exe.display(), config.display());
		let (run, _) = RegKey::predef(HKEY_CURRENT_USER)
			.create_subkey(RUN_KEY)
			.context("opening the HKCU Run key")?;
		run.set_value(VALUE, &command).context("writing the autostart entry")?;

		// A previous version may have left a scheduled task; remove it so we don't run twice.
		let _ = Command::new("schtasks")
			.args(["/Delete", "/TN", LEGACY_TASK, "/F"])
			.no_window()
			.status();

		// Start it now (detached, no window, no inherited handles) so the machine is
		// reachable immediately rather than only after the next logon.
		Command::new(exe)
			.arg("run")
			.arg("--config")
			.arg(config)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.no_window()
			.spawn()
			.context("starting the agent")?;
		println!("Installed per-user autostart (HKCU Run) and started the agent.");
		Ok(())
	}

	pub fn uninstall() -> Result<()> {
		if let Ok(run) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_WRITE) {
			let _ = run.delete_value(VALUE);
		}
		let _ = Command::new("schtasks")
			.args(["/Delete", "/TN", LEGACY_TASK, "/F"])
			.no_window()
			.status();
		// Stop a running agent (best-effort).
		let _ = Command::new("taskkill")
			.args(["/IM", "libretether-agent.exe", "/F"])
			.no_window()
			.status();
		println!("Removed per-user autostart (HKCU Run).");
		Ok(())
	}
}
