//! Small host-side utilities shared across the LibreTether binaries: wall-clock
//! time, a unified SIGINT/SIGTERM shutdown future, a `which`-style binary probe,
//! and a `NoWindow` trait that suppresses the console window child processes
//! would otherwise pop up on Windows.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current unix time in seconds (0 if the clock is before the epoch).
pub fn now_secs() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

/// Resolve on the first SIGINT/SIGTERM (Ctrl+C on Windows) so a process can shut
/// down cleanly — used by the agent and the relay so `docker stop` / a service
/// stop end gracefully instead of being force-killed.
pub async fn shutdown_signal() {
	#[cfg(unix)]
	{
		use tokio::signal::unix::{signal, SignalKind};
		let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
		let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
		tokio::select! {
			_ = term.recv() => {}
			_ = int.recv() => {}
		}
	}
	#[cfg(not(unix))]
	{
		let _ = tokio::signal::ctrl_c().await;
	}
}

/// True if `bin` is found on `PATH` (via `which`). Unix-only; returns false on
/// other platforms (all callers are gated to Linux).
#[cfg(unix)]
pub fn which(bin: &str) -> bool {
	std::process::Command::new("which")
		.arg(bin)
		.output()
		.map(|o| o.status.success())
		.unwrap_or(false)
}

#[cfg(not(unix))]
pub fn which(_bin: &str) -> bool {
	false
}

/// `CREATE_NO_WINDOW` process-creation flag.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Suppress the console window a child process would otherwise pop up on Windows.
///
/// A GUI-subsystem binary (the agent) that launches a console program — `tailscale`,
/// `powershell`, an exec target — makes Windows allocate a fresh console and flash
/// its window on the guest's screen. `CREATE_NO_WINDOW` prevents that. A no-op on
/// other platforms. Implemented for both `std` and `tokio` `Command`s.
pub trait NoWindow {
	fn no_window(&mut self) -> &mut Self;
}

impl NoWindow for std::process::Command {
	#[cfg(windows)]
	fn no_window(&mut self) -> &mut Self {
		use std::os::windows::process::CommandExt;
		self.creation_flags(CREATE_NO_WINDOW)
	}

	#[cfg(not(windows))]
	fn no_window(&mut self) -> &mut Self {
		self
	}
}

impl NoWindow for tokio::process::Command {
	#[cfg(windows)]
	fn no_window(&mut self) -> &mut Self {
		self.creation_flags(CREATE_NO_WINDOW)
	}

	#[cfg(not(windows))]
	fn no_window(&mut self) -> &mut Self {
		self
	}
}
