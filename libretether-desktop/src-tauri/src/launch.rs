//! Shared helpers for launching the host's external viewer/terminal programs
//! (RDP viewers, SSH terminals). Centralizes the template split, the spawn +
//! error mapping, and URL percent-encoding so `rdp.rs` and `ssh.rs` don't each
//! re-implement (and risk diverging on) them.

use std::process::Command;

use crate::error::{AppError, AppResult};

/// Split an operator-entered launcher template into `(program, remaining args)`.
/// The program is the first whitespace-delimited token; the rest are arguments.
/// Errors if the template is blank.
///
/// Linux-only: only the Linux launchers accept a custom command template (macOS
/// and Windows use fixed system clients).
#[cfg(target_os = "linux")]
pub fn split_template(template: &str) -> AppResult<(&str, std::str::SplitWhitespace<'_>)> {
	let mut tokens = template.split_whitespace();
	let bin = tokens.next().ok_or_else(|| AppError::msg("empty launcher command"))?;
	Ok((bin, tokens))
}

/// Spawn `cmd`, mapping a launch failure to a readable error tagged with `label`.
pub fn spawn(mut cmd: Command, label: &str) -> AppResult<()> {
	cmd.spawn()
		.map(|_| ())
		.map_err(|e| AppError::msg(format!("launching {label}: {e}")))
}

/// Like [`spawn`], but capture the child's stdout+stderr and forward each line to
/// the in-app log under `source` — so the launched viewer's diagnostics (notably
/// FreeRDP's `WLog` output) show up on the Logs page instead of vanishing into a
/// GUI build's missing console. The viewer stays detached: we drain its pipes and
/// reap it on background threads, but never wait on or kill it.
pub fn spawn_logged(mut cmd: Command, label: &str, source: &'static str) -> AppResult<()> {
	use std::process::Stdio;
	cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
	let mut child = cmd
		.spawn()
		.map_err(|e| AppError::msg(format!("launching {label}: {e}")))?;
	if let Some(out) = child.stdout.take() {
		std::thread::spawn(move || drain_to_log(out, source));
	}
	if let Some(err) = child.stderr.take() {
		std::thread::spawn(move || drain_to_log(err, source));
	}
	// Reap the process when it exits so it doesn't linger as a zombie. The pipe
	// handles are owned by the drain threads, so `wait()` just collects the pid.
	std::thread::spawn(move || {
		let _ = child.wait();
	});
	Ok(())
}

/// Read `reader` line by line, logging each non-empty line under `source` at a
/// level inferred from a bracketed `[LEVEL]` tag (FreeRDP/WLog style: `[INFO]`,
/// `[WARN]`, `[ERROR]`, …), defaulting to info.
fn drain_to_log<R: std::io::Read>(reader: R, source: &'static str) {
	use std::io::{BufRead, BufReader};
	for line in BufReader::new(reader).lines() {
		let Ok(line) = line else { break };
		let line = line.trim_end();
		if line.is_empty() {
			continue;
		}
		crate::logbook::record(log_level_of(line), source, line);
	}
}

fn log_level_of(line: &str) -> libretether_protocol::LogLevel {
	use libretether_protocol::LogLevel;
	if line.contains("[ERROR]") || line.contains("[FATAL]") {
		LogLevel::Error
	} else if line.contains("[WARN]") {
		LogLevel::Warn
	} else if line.contains("[TRACE]") {
		LogLevel::Trace
	} else if line.contains("[DEBUG]") {
		LogLevel::Debug
	} else {
		LogLevel::Info
	}
}

/// Percent-encode `s` for a URL userinfo/query component (RFC 3986): unreserved
/// characters pass through, everything else is `%XX`-escaped. Used so a username
/// or password embedded in an `rdp://` URL can't alter the URL's structure (a `@`,
/// `:`, `/`, `?`, `#`, `\` or space would otherwise be parsed as a delimiter), no
/// longer relying on the upstream char allowlist alone to keep the URL well-formed.
///
/// Linux/macOS-only: those platforms launch RDP via an `rdp://` URL; Windows uses
/// `mstsc`/`cmdkey` arguments instead, so no URL encoding is needed there.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn percent_encode(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for &b in s.as_bytes() {
		match b {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
			_ => out.push_str(&format!("%{b:02X}")),
		}
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	#[cfg(any(target_os = "linux", target_os = "macos"))]
	#[test]
	fn percent_encode_passes_unreserved_and_escapes_the_rest() {
		assert_eq!(percent_encode("Abc-123_.~"), "Abc-123_.~");
		// URL-significant characters that would otherwise break an rdp:// userinfo.
		assert_eq!(percent_encode("a@b:c/d"), "a%40b%3Ac%2Fd");
		assert_eq!(percent_encode("p ss"), "p%20ss");
		assert_eq!(percent_encode("DOMAIN\\user"), "DOMAIN%5Cuser");
		assert_eq!(percent_encode("a&b?c#d"), "a%26b%3Fc%23d");
	}

	#[cfg(target_os = "linux")]
	#[test]
	fn split_template_separates_program_from_args() {
		let (bin, rest) = split_template("gnome-terminal -- --x").unwrap();
		assert_eq!(bin, "gnome-terminal");
		assert_eq!(rest.collect::<Vec<_>>(), vec!["--", "--x"]);
		// Leading/trailing whitespace is ignored; a lone program has no args.
		let (bin, rest) = split_template("  remmina  ").unwrap();
		assert_eq!(bin, "remmina");
		assert_eq!(rest.count(), 0);
		assert!(split_template("   ").is_err());
	}
}
