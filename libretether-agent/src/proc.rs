//! Subprocess helpers.

/// Suppress the console window a child process would otherwise pop up on Windows.
///
/// The agent runs windowless (GUI subsystem), so any console program it launches
/// — `tailscale`, `powershell`, an exec target — makes Windows allocate a fresh
/// console and briefly flash its window on the guest's screen. `CREATE_NO_WINDOW`
/// prevents that. A no-op on other platforms.
pub trait NoWindow {
	fn no_window(&mut self) -> &mut Self;
}

impl NoWindow for std::process::Command {
	#[cfg(windows)]
	fn no_window(&mut self) -> &mut Self {
		use std::os::windows::process::CommandExt;
		const CREATE_NO_WINDOW: u32 = 0x0800_0000;
		self.creation_flags(CREATE_NO_WINDOW)
	}

	#[cfg(not(windows))]
	fn no_window(&mut self) -> &mut Self {
		self
	}
}
