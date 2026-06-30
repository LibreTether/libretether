//! In-memory capture of the controller's own log lines, for the UI's Logs page.
//!
//! The controller used to log with bare `eprintln!`, which goes nowhere on a GUI
//! build (no attached console). This keeps the stderr output *and* retains a
//! bounded ring of recent lines the UI can query ([`entries`], via the
//! `get_controller_logs` command) and subscribe to live (the [`EVENT_LOG`]
//! event) — so an operator can see what the controller is doing without a
//! terminal. Agent logs fetched over the link are normalised into the same
//! [`LogEntry`] shape, with the client name as their `source`.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use libretether_protocol::LogLevel;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

/// Tauri event carrying one freshly-recorded [`LogEntry`] to the Logs page.
pub const EVENT_LOG: &str = "logs:entry";

/// How many recent controller lines to retain. Bounded so a long-lived controller
/// can't grow the buffer without limit; older lines are evicted.
const CAP: usize = 2000;

/// One captured log line. `source` names the subsystem ("controller", "tunnel");
/// agent logs reuse this shape with the client's name as `source`.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
	pub ts_secs: u64,
	pub level: LogLevel,
	pub source: String,
	pub message: String,
}

static APP: OnceLock<AppHandle> = OnceLock::new();

fn book() -> &'static Mutex<VecDeque<LogEntry>> {
	static BOOK: OnceLock<Mutex<VecDeque<LogEntry>>> = OnceLock::new();
	BOOK.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAP)))
}

/// Hand the logbook an app handle so new lines reach the UI live. Called once at
/// startup; lines recorded before it still land in the buffer (and seed the page).
pub fn set_app(app: AppHandle) {
	let _ = APP.set(app);
}

fn now_secs() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

/// Record a controller log line: mirror it to stderr (so `cargo run`/journald
/// still show it), retain it in the ring, and emit it to the Logs page.
pub fn record(level: LogLevel, source: &str, message: &str) {
	record_at(now_secs(), level, source, message);
}

/// Like [`record`] but with an explicit timestamp, for lines pulled from another
/// host (the relay, an agent) whose `ts_secs` has been re-anchored to our clock —
/// so they retain their true age instead of all collapsing to "now" on ingest.
pub fn record_at(ts_secs: u64, level: LogLevel, source: &str, message: &str) {
	eprintln!("[libretether] {message}");
	let entry = LogEntry {
		ts_secs,
		level,
		source: source.to_string(),
		message: message.to_string(),
	};
	{
		let mut book = book().lock().unwrap();
		if book.len() == CAP {
			book.pop_front();
		}
		book.push_back(entry.clone());
	}
	if let Some(app) = APP.get() {
		let _ = app.emit(EVENT_LOG, entry);
	}
}

pub fn info(source: &str, message: &str) {
	record(LogLevel::Info, source, message);
}

pub fn warn(source: &str, message: &str) {
	record(LogLevel::Warn, source, message);
}

pub fn error(source: &str, message: &str) {
	record(LogLevel::Error, source, message);
}

/// Record a debug-level line: fine-grained per-stream/per-step detail (stream
/// opened/closed, handshake sub-steps, tunnel accepts) that the Logs page can
/// filter out. Reach for [`info`] for connection-level milestones.
pub fn debug(source: &str, message: &str) {
	record(LogLevel::Debug, source, message);
}

/// The most recent `max` lines (all when `None`), oldest first — used to seed the
/// Logs page before live events take over.
pub fn entries(max: Option<usize>) -> Vec<LogEntry> {
	let book = book().lock().unwrap();
	let take = max.unwrap_or(book.len()).min(book.len());
	book.iter().skip(book.len() - take).cloned().collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	// `entries(Some(n))` returns the newest n lines, oldest-first. Asserted on the
	// tail this test just pushed, so it's robust to anything already in the global
	// ring.
	#[test]
	fn entries_returns_the_newest_lines_oldest_first() {
		record(LogLevel::Info, "test", "alpha");
		record(LogLevel::Warn, "test", "bravo");
		record(LogLevel::Error, "test", "charlie");
		let tail = entries(Some(3));
		assert_eq!(
			tail.iter().map(|e| e.message.as_str()).collect::<Vec<_>>(),
			["alpha", "bravo", "charlie"],
		);
		assert_eq!(tail[2].level, LogLevel::Error);
	}
}
