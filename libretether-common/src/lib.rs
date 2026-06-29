//! Small host-side utilities shared across the LibreTether binaries: wall-clock
//! time, a unified SIGINT/SIGTERM shutdown future, a `which`-style binary probe,
//! address/port normalization, a reconnect backoff, bidirectional stream piping,
//! and a `NoWindow` trait that suppresses the console window child processes
//! would otherwise pop up on Windows.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// Current unix time in seconds (0 if the clock is before the epoch).
pub fn now_secs() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

/// Append `:default_port` to `addr` when it carries no port. Correctly handles
/// bare IPv6 literals (which contain colons but no port) by bracketing them, so
/// `2001:db8::1` becomes `[2001:db8::1]:port` rather than being mistaken for a
/// `host:port` pair.
pub fn with_default_port(addr: &str, default_port: u16) -> String {
	let addr = addr.trim();
	// Bracketed IPv6: "[::1]" needs a port, "[::1]:80" already has one.
	if addr.starts_with('[') {
		let has_port = addr.rsplit_once(']').is_some_and(|(_, rest)| rest.starts_with(':'));
		return if has_port {
			addr.to_string()
		} else {
			format!("{addr}:{default_port}")
		};
	}
	// A bare IPv6 literal (multiple colons, parses as an address) — bracket it.
	if addr.parse::<std::net::Ipv6Addr>().is_ok() {
		return format!("[{addr}]:{default_port}");
	}
	// A hostname or IPv4: a single colon means a port is already present.
	if addr.contains(':') {
		addr.to_string()
	} else {
		format!("{addr}:{default_port}")
	}
}

/// Capped exponential backoff for reconnect loops: each [`Backoff::next_delay`]
/// returns the current delay and doubles it (saturating at `max`); [`Backoff::reset`]
/// drops it back to the floor after a healthy attempt.
pub struct Backoff {
	current: u64,
	max: u64,
}

impl Backoff {
	/// Start at a 1-second delay, doubling up to `max_secs`.
	pub fn new(max_secs: u64) -> Self {
		Self {
			current: 1,
			max: max_secs.max(1),
		}
	}

	/// Reset to the 1-second floor (call after a connection stayed healthy).
	pub fn reset(&mut self) {
		self.current = 1;
	}

	/// The delay to wait before the next attempt, then double it for the one after.
	pub fn next_delay(&mut self) -> Duration {
		let wait = self.current.min(self.max);
		self.current = (self.current * 2).min(self.max);
		Duration::from_secs(wait)
	}
}

/// Copy bytes in both directions between two duplex byte streams until both
/// directions close: data read from endpoint A is written to endpoint B and vice
/// versa. Each direction's write half is shut down when its source ends, then we
/// wait for *both* — tearing both halves down on the first EOF (e.g. a shared
/// `select!`) truncates a request/response or live session. Shared by the relay's
/// routing, the agent's RDP/SSH tunnel, and the controller's loopback forwarder.
pub async fn pipe_bidirectional<AR, AW, BR, BW>(mut a_read: AR, mut a_write: AW, mut b_read: BR, mut b_write: BW)
where
	AR: AsyncRead + Unpin,
	AW: AsyncWrite + Unpin,
	BR: AsyncRead + Unpin,
	BW: AsyncWrite + Unpin,
{
	let a_to_b = async {
		let _ = tokio::io::copy(&mut a_read, &mut b_write).await;
		let _ = b_write.shutdown().await;
	};
	let b_to_a = async {
		let _ = tokio::io::copy(&mut b_read, &mut a_write).await;
		let _ = a_write.shutdown().await;
	};
	tokio::join!(a_to_b, b_to_a);
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn with_default_port_handles_hosts_ipv4_and_ipv6() {
		// Host / IPv4 without a port get the default appended.
		assert_eq!(with_default_port("ctl.example", 47600), "ctl.example:47600");
		assert_eq!(with_default_port("10.0.0.5", 47600), "10.0.0.5:47600");
		// An explicit port is preserved.
		assert_eq!(with_default_port("ctl.example:1234", 47600), "ctl.example:1234");
		assert_eq!(with_default_port("10.0.0.5:22", 47600), "10.0.0.5:22");
		// Bare IPv6 literals are bracketed and given the default port.
		assert_eq!(with_default_port("::1", 47600), "[::1]:47600");
		assert_eq!(with_default_port("2001:db8::1", 47600), "[2001:db8::1]:47600");
		// Bracketed IPv6 with or without a port.
		assert_eq!(with_default_port("[::1]", 47600), "[::1]:47600");
		assert_eq!(with_default_port("[2001:db8::1]:9000", 47600), "[2001:db8::1]:9000");
		// Surrounding whitespace is trimmed.
		assert_eq!(with_default_port("  host  ", 47600), "host:47600");
	}

	#[test]
	fn backoff_grows_geometrically_caps_and_resets() {
		let mut b = Backoff::new(5);
		assert_eq!(b.next_delay(), Duration::from_secs(1));
		assert_eq!(b.next_delay(), Duration::from_secs(2));
		assert_eq!(b.next_delay(), Duration::from_secs(4));
		// Capped at max.
		assert_eq!(b.next_delay(), Duration::from_secs(5));
		assert_eq!(b.next_delay(), Duration::from_secs(5));
		// Reset drops back to the floor.
		b.reset();
		assert_eq!(b.next_delay(), Duration::from_secs(1));
	}

	#[tokio::test]
	async fn pipe_bidirectional_copies_both_directions() {
		use tokio::io::AsyncReadExt;

		// `a_ext`/`b_ext` are the two external endpoints; the pipe joins their
		// internal halves.
		let (mut a_ext, a_int) = tokio::io::duplex(64);
		let (mut b_ext, b_int) = tokio::io::duplex(64);
		let (a_read, a_write) = tokio::io::split(a_int);
		let (b_read, b_write) = tokio::io::split(b_int);
		let pump = tokio::spawn(pipe_bidirectional(a_read, a_write, b_read, b_write));

		// Send a message each way, then close both writers so the pump drains + ends.
		a_ext.write_all(b"a->b").await.unwrap();
		b_ext.write_all(b"b->a").await.unwrap();
		a_ext.shutdown().await.unwrap();
		b_ext.shutdown().await.unwrap();

		let mut from_a = Vec::new();
		b_ext.read_to_end(&mut from_a).await.unwrap();
		let mut from_b = Vec::new();
		a_ext.read_to_end(&mut from_b).await.unwrap();
		assert_eq!(from_a, b"a->b");
		assert_eq!(from_b, b"b->a");
		pump.await.unwrap();
	}
}
