//! In-memory rings of the relay's own log lines.
//!
//! The relay runs headless on a public host, so its diagnostics normally only
//! reach stderr (journald / `docker logs`). This keeps that stderr output *and*
//! retains bounded rings the connected controllers can pull over the control
//! connection ([`libretether_protocol::relay::RelayRequest::FetchLogs`]), so an
//! operator can read the relay's activity from the desktop app's Logs page without
//! shelling into the relay host. Lines reuse the protocol's [`LogLine`] /
//! [`LogsResult`] shape, so the controller normalises them exactly like agent logs.
//!
//! Because the relay is multi-tenant, logs are **scoped**: server-wide lines
//! (startup, capacity shedding, the portal) go to the process-global [`Log`] via the
//! free [`info`]/[`warn`]/[`debug`] functions, while per-connection lines that would
//! reveal a tenant's agents go to that tenant's own [`Log`]. A tenant's controller
//! only ever fetches its own tenant's ring, so one tenant never sees another's
//! activity. Every line is still mirrored to stderr for the operator.

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
	/// Count of all lines ever recorded (including evicted ones), so the controller
	/// can fetch incrementally. The seq of the oldest retained line is
	/// `next_seq - lines.len()`; line `i` in `lines` has seq `next_seq - lines.len() + i`.
	next_seq: u64,
}

/// A bounded, fetchable log ring with a stderr `tag`. There is one process-global
/// `Log` (untagged, for server-wide lines) and one per tenant (tagged with a short
/// tenant label, so `docker logs` attributes each line, and fetchable only by that
/// tenant's controller).
pub struct Log {
	ring: Mutex<Ring>,
	tag: String,
}

impl Log {
	/// A fresh, empty log. `tag` is prefixed to stderr lines (e.g. a tenant label);
	/// pass an empty string for the untagged server-wide log.
	pub fn new(tag: impl Into<String>) -> Self {
		Self {
			ring: Mutex::new(Ring::default()),
			tag: tag.into(),
		}
	}

	/// Record a line at `level`: mirror it to stderr (so journald / `docker logs`
	/// still show it) and push it onto the ring the controller can fetch.
	pub fn record(&self, level: LogLevel, message: &str) {
		if self.tag.is_empty() {
			eprintln!("[libretether-relay] {message}");
		} else {
			eprintln!("[libretether-relay] [{}] {message}", self.tag);
		}
		let mut ring = self.ring.lock().unwrap();
		if ring.lines.len() >= CAP {
			ring.lines.pop_front();
			ring.dropped = true;
		}
		ring.lines.push_back(LogLine {
			ts_secs: now_secs(),
			level,
			message: message.to_string(),
		});
		ring.next_seq += 1;
	}

	pub fn info(&self, message: &str) {
		self.record(LogLevel::Info, message);
	}

	pub fn warn(&self, message: &str) {
		self.record(LogLevel::Warn, message);
	}

	/// Record a debug-level line: fine-grained per-connection/per-stream detail
	/// (connection accepted, stream routed, peer rejected) that the controller's
	/// Logs page can filter out. Reach for [`Log::info`] for higher-level milestones.
	pub fn debug(&self, message: &str) {
		self.record(LogLevel::Debug, message);
	}

	/// See [`Ring::snapshot_after`].
	pub fn snapshot_after(&self, after_seq: Option<u64>) -> LogsResult {
		self.ring.lock().unwrap().snapshot_after(after_seq)
	}
}

/// The process-global, server-wide log. Server lifecycle and cross-tenant lines go
/// here (and to stderr); it is never exposed over a tenant's control connection.
fn global() -> &'static Log {
	static GLOBAL: OnceLock<Log> = OnceLock::new();
	GLOBAL.get_or_init(|| Log::new(""))
}

pub fn info(message: &str) {
	global().info(message);
}

pub fn warn(message: &str) {
	global().warn(message);
}

/// Record a server-wide debug line on the global log (see [`Log::debug`]).
pub fn debug(message: &str) {
	global().debug(message);
}

impl Ring {
	/// Lines recorded after the `after_seq` cursor (all retained lines when `None`),
	/// oldest first, stamped with the relay's current clock so the controller can
	/// re-anchor the line timestamps to its own (the relay host may be in another
	/// timezone or have a skewed clock). The returned [`LogsResult::next_seq`] is the
	/// cursor to pass back next poll.
	///
	/// `dropped` means the requested history is partial: either the ring evicted lines
	/// the cursor hadn't yet seen, or (on a relay restart, detected when the cursor is
	/// ahead of our total) the cursor refers to a previous relay process and we resend
	/// everything we have so the controller resyncs.
	fn snapshot_after(&self, after_seq: Option<u64>) -> LogsResult {
		let total = self.next_seq;
		let len = self.lines.len() as u64;
		let start = total - len; // seq of the oldest retained line

		let (skip, dropped) = match after_seq {
			// Full snapshot: everything retained.
			None => (0, self.dropped),
			// Cursor ahead of our total ⇒ it belongs to a previous (restarted) relay
			// process whose seqs don't apply here — resend everything and flag it.
			Some(cursor) if cursor > total => (0, true),
			Some(cursor) => {
				// We want lines with seq > cursor; the first such line is at index
				// `(cursor + 1) - start` in the retained window (clamped to it).
				let want_from = cursor + 1;
				let skip = want_from.saturating_sub(start).min(len);
				// If the cursor's next expected line predates our oldest retained line,
				// lines were evicted before the controller could read them.
				(skip, want_from < start)
			}
		};

		LogsResult {
			lines: self.lines.iter().skip(skip as usize).cloned().collect(),
			dropped,
			now_secs: now_secs(),
			next_seq: total,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A local ring holding `msgs` as consecutive lines (seq `0..msgs.len()`). Used so
	/// the snapshot-cursor tests are deterministic — the global `record`/`snapshot`
	/// path shares one process-wide ring across parallel tests, which would race exact
	/// assertions.
	fn ring_with(msgs: &[&str]) -> Ring {
		let mut ring = Ring::default();
		for m in msgs {
			ring.lines.push_back(LogLine {
				ts_secs: 0,
				level: LogLevel::Info,
				message: (*m).to_string(),
			});
			ring.next_seq += 1;
		}
		ring
	}

	fn msgs(r: &LogsResult) -> Vec<&str> {
		r.lines.iter().map(|l| l.message.as_str()).collect()
	}

	// A full snapshot (`None`) returns every retained line oldest-first, with a
	// `next_seq` cursor past the last line.
	#[test]
	fn snapshot_none_returns_all_lines_with_a_cursor() {
		let r = ring_with(&["alpha", "bravo", "charlie"]).snapshot_after(None);
		assert_eq!(msgs(&r), ["alpha", "bravo", "charlie"]);
		assert_eq!(r.next_seq, 3);
	}

	// An incremental snapshot returns only the lines after the cursor; re-fetching
	// with the latest cursor returns nothing new.
	#[test]
	fn snapshot_after_returns_only_newer_lines() {
		let ring = ring_with(&["one", "two", "three"]);
		// Lines have seq 0,1,2 — after cursor 1 only seq 2 ("three") is new.
		let fresh = ring.snapshot_after(Some(1));
		assert_eq!(msgs(&fresh), ["three"]);
		assert_eq!(fresh.next_seq, 3);
		assert!(!fresh.dropped);
		// Caught up at the latest cursor: nothing new.
		assert!(ring.snapshot_after(Some(3)).lines.is_empty());
	}

	// A cursor from a previous (restarted) relay — ahead of our total — makes the
	// relay resend everything it has and flag the history as partial.
	#[test]
	fn snapshot_after_a_future_cursor_resends_everything() {
		let resent = ring_with(&["post-restart"]).snapshot_after(Some(10_000));
		assert!(resent.dropped, "a future cursor signals a gap/restart");
		assert_eq!(resent.next_seq, 1);
		assert_eq!(msgs(&resent), ["post-restart"]);
	}

	// When the ring has evicted lines the cursor never saw, the snapshot returns the
	// retained tail and flags `dropped` so the controller knows history is partial.
	#[test]
	fn snapshot_after_an_evicted_cursor_flags_dropped() {
		// Simulate a ring that recorded 10 lines but only retains the last 3 (seq 7,8,9).
		let mut ring = ring_with(&["seven", "eight", "nine"]);
		ring.next_seq = 10;
		ring.dropped = true;
		// Cursor 2's next expected line (seq 3) was evicted → partial, return the tail.
		let r = ring.snapshot_after(Some(2));
		assert!(r.dropped);
		assert_eq!(msgs(&r), ["seven", "eight", "nine"]);
		assert_eq!(r.next_seq, 10);
	}
}
