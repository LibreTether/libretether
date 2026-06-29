//! Install/uninstall the agent as an always-on background service so the
//! machine stays reachable whenever it's powered on.
//!
//! Screen capture and input injection must run inside the user's graphical
//! session, so on every platform we install a *per-user* service (systemd user
//! unit / LaunchAgent / logon scheduled task), not a system-wide daemon.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};

fn exe() -> Result<PathBuf> {
	std::env::current_exe().context("locating the agent executable")
}

pub fn install(config_path: &Path) -> Result<()> {
	platform::install(&exe()?, config_path)
}

pub fn uninstall() -> Result<()> {
	platform::uninstall()
}

fn run(cmd: &mut Command) -> Result<()> {
	let status = cmd.status().with_context(|| format!("running {cmd:?}"))?;
	if !status.success() {
		return Err(anyhow!("command failed ({status}): {cmd:?}"));
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
			 ExecStart={exe} run --config {config}\n\
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
	use super::*;

	const TASK: &str = "LibreTetherAgent";

	pub fn install(exe: &Path, config: &Path) -> Result<()> {
		// Run at logon in the interactive session so capture/input work.
		let cmd = format!("\"{}\" run --config \"{}\"", exe.display(), config.display());
		run(Command::new("schtasks").args([
			"/Create", "/TN", TASK, "/SC", "ONLOGON", "/RL", "LIMITED", "/F", "/TR", &cmd,
		]))?;
		let _ = Command::new("schtasks").args(["/Run", "/TN", TASK]).status();
		println!("Installed logon scheduled task: {TASK}");
		Ok(())
	}

	pub fn uninstall() -> Result<()> {
		let _ = Command::new("schtasks").args(["/End", "/TN", TASK]).status();
		let _ = Command::new("schtasks").args(["/Delete", "/TN", TASK, "/F"]).status();
		println!("Removed scheduled task: {TASK}");
		Ok(())
	}
}
