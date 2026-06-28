//! Host facts the controller likes to show: identity, uptime, boot time.

use std::time::{SystemTime, UNIX_EPOCH};

use tether_protocol::HostInfo;

pub fn host_info() -> HostInfo {
	HostInfo {
		hostname: whoami::fallible::hostname().unwrap_or_else(|_| "unknown".to_string()),
		os: os_label(),
		arch: std::env::consts::ARCH.to_string(),
		username: whoami::username(),
	}
}

/// A friendly OS label, e.g. "Ubuntu 24.04" / "Windows" / "macOS".
fn os_label() -> String {
	let distro = whoami::distro();
	if distro.is_empty() {
		std::env::consts::OS.to_string()
	} else {
		distro
	}
}

/// Current unix time in seconds.
pub fn now_secs() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

/// Best-effort machine boot time (unix seconds). Linux-only for now.
pub fn boot_time_secs() -> Option<u64> {
	#[cfg(target_os = "linux")]
	{
		let uptime = std::fs::read_to_string("/proc/uptime").ok()?;
		let secs: f64 = uptime.split_whitespace().next()?.parse().ok()?;
		Some(now_secs().saturating_sub(secs as u64))
	}
	#[cfg(not(target_os = "linux"))]
	{
		None
	}
}
