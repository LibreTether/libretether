//! In-memory ring of the relay's own log lines.
//!
//! The relay runs headless on a public host, so its diagnostics normally only
//! reach stderr (journald / `docker logs`). This keeps that stderr output *and*
//! retains a bounded ring the connected controller can pull over the control
//! connection ([`libretether_protocol::relay::RelayRequest::FetchLogs`]), so an
//! operator can read the relay's activity from the desktop app's Logs page without
//! shelling into the relay host. Lines reuse the protocol's [`LogLine`] /
//! [`LogsResult`] shape, so the controller normalises them exactly like agent logs.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use libretether_common::now_secs;
use libretether_protocol::{LogLevel, LogLine, LogsResult};

/// How many recent lines to retain. Bounded so a long-lived relay can't grow the
/// buffer without limit; older lines are evicted and the snapshot's `dropped` flag
/// is set so the controller can show the returned history is partial.
const CAP: usize = 2000;

#[derive(Default)]
struct Ring {
	lines: VecDeque<LogLine>,
	dropped: bool,
}

fn ring() -> &'static Mutex<Ring> {
	static RING: OnceLock<Mutex<Ring>> = OnceLock::new();
	RING.get_or_init(|| Mutex::new(Ring::default()))
}

/// Record a line at `level`: mirror it to stderr (so journald / `docker logs` still
/// show it) and push it onto the ring the controller can fetch.
pub fn record(level: LogLevel, message: &str) {
	eprintln!("[libretether-relay] {message}");
	let mut ring = ring().lock().unwrap();
	if ring.lines.len() >= CAP {
		ring.lines.pop_front();
		ring.dropped = true;
	}
	ring.lines.push_back(LogLine {
		ts_secs: now_secs(),
		level,
		message: message.to_string(),
	});
}

pub fn info(message: &str) {
	record(LogLevel::Info, message);
}

pub fn warn(message: &str) {
	record(LogLevel::Warn, message);
}

/// The most recent `max` lines (all when `None`), oldest first, stamped with the
/// relay's current clock so the controller can re-anchor the line timestamps to its
/// own (the relay host may be in another timezone or have a skewed clock).
pub fn snapshot(max: Option<usize>) -> LogsResult {
	let ring = ring().lock().unwrap();
	let take = max.unwrap_or(ring.lines.len()).min(ring.lines.len());
	LogsResult {
		lines: ring.lines.iter().skip(ring.lines.len() - take).cloned().collect(),
		dropped: ring.dropped,
		now_secs: now_secs(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// `snapshot(Some(n))` returns the newest n lines, oldest-first — asserted on the
	// tail this test pushes, so it's robust to anything already in the global ring.
	#[test]
	fn snapshot_returns_the_newest_lines_oldest_first() {
		record(LogLevel::Info, "alpha");
		record(LogLevel::Warn, "bravo");
		record(LogLevel::Info, "charlie");
		let tail = snapshot(Some(3));
		assert_eq!(
			tail.lines.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(),
			["alpha", "bravo", "charlie"],
		);
		assert_eq!(tail.lines[1].level, LogLevel::Warn);
	}
}
