//! One-shot control-plane request handlers (status, exec, screenshot).

use std::process::Stdio;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use libretether_protocol::{
	AgentStatus, ControlRequest, ControlResponse, ExecResult, LogLevel, PortProbe, ScreenshotResult, SshInfo,
	DEFAULT_EXEC_TIMEOUT_SECS, MAX_EXEC_TIMEOUT_SECS,
};
use tokio::process::Command;

use crate::capture;
use crate::host;

/// Process start time, captured once so we can report agent uptime.
static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
static START_UNIX: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Record the process start instant. Call once at startup.
pub fn mark_start() {
	let _ = START.set(Instant::now());
	let _ = START_UNIX.set(host::now_secs());
}

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn handle(req: ControlRequest) -> ControlResponse {
	crate::net::debug(&format!("handling control request: {}", request_label(&req)));
	let resp = dispatch(req).await;
	if let ControlResponse::Error { message } = &resp {
		crate::net::log_at(LogLevel::Warn, &format!("control request failed: {message}"));
	}
	resp
}

/// A short, secret-free label for a control request, for the audit log. `Exec`
/// names the program and arg count but never the argument values (they can carry
/// secrets); mirrors the controller-side exec logging.
fn request_label(req: &ControlRequest) -> String {
	match req {
		ControlRequest::Ping => "ping".into(),
		ControlRequest::Status => "status".into(),
		ControlRequest::Exec { program, args, .. } => format!("exec {program} ({} args)", args.len()),
		ControlRequest::Screenshot { display } => format!("screenshot (display {})", display.unwrap_or(0)),
		ControlRequest::EnableRdp => "enable rdp".into(),
		ControlRequest::FetchLogs { .. } => "fetch logs".into(),
		ControlRequest::ProbePort { port } => format!("probe port {port}"),
		ControlRequest::EnableSsh => "enable ssh".into(),
	}
}

async fn dispatch(req: ControlRequest) -> ControlResponse {
	match req {
		ControlRequest::Ping => ControlResponse::Pong,
		ControlRequest::Status => ControlResponse::Status(status()),
		ControlRequest::Exec {
			program,
			args,
			timeout_secs,
		} => match exec(program, args, timeout_secs).await {
			Ok(r) => {
				crate::net::debug(&format!("exec finished: exit {:?} in {} ms", r.code, r.duration_ms));
				ControlResponse::Exec(r)
			}
			Err(e) => ControlResponse::Error { message: e },
		},
		ControlRequest::Screenshot { display } => match screenshot(display.unwrap_or(0)).await {
			Ok(r) => {
				crate::net::debug(&format!(
					"screenshot captured: {}x{} (display {})",
					r.width, r.height, r.display
				));
				ControlResponse::Screenshot(r)
			}
			Err(e) => ControlResponse::Error { message: e },
		},
		ControlRequest::EnableRdp => match tokio::task::spawn_blocking(crate::rdp::enable).await {
			Ok(Ok(info)) => {
				crate::net::log(&format!("RDP enabled ({} on port {})", info.backend, info.port));
				ControlResponse::Rdp(info)
			}
			Ok(Err(e)) => ControlResponse::Error { message: e },
			Err(e) => ControlResponse::Error {
				message: format!("rdp task failed: {e}"),
			},
		},
		ControlRequest::FetchLogs { max_lines } => {
			ControlResponse::Logs(crate::net::recent_logs(max_lines.map(|n| n as usize)))
		}
		ControlRequest::ProbePort { port } => ControlResponse::PortReachable(PortProbe {
			reachable: probe_port(port).await,
		}),
		ControlRequest::EnableSsh => match crate::ssh_server::ensure().await {
			Ok(embedded) => ControlResponse::Ssh(SshInfo {
				port: embedded.port,
				username: host::host_info().username,
				private_key: embedded.private_key_openssh,
			}),
			Err(e) => ControlResponse::Error {
				message: format!("starting embedded SSH server: {e}"),
			},
		},
	}
}

/// Best-effort TCP reachability check for a loopback service on the agent host
/// (e.g. the client's own SSH server). A short timeout keeps a filtered/dropped
/// port from hanging the control request.
async fn probe_port(port: u16) -> bool {
	let connect = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port));
	matches!(tokio::time::timeout(Duration::from_secs(3), connect).await, Ok(Ok(_)))
}

fn status() -> AgentStatus {
	let uptime_secs = START.get().map(|s| s.elapsed().as_secs()).unwrap_or(0);
	AgentStatus {
		host: host::host_info(),
		agent_version: AGENT_VERSION.to_string(),
		uptime_secs,
		started_at: START_UNIX.get().copied().unwrap_or_else(host::now_secs),
		boot_time_secs: host::boot_time_secs(),
		displays: capture::display_count(),
		tailscale_ip: host::tailscale_ip(),
	}
}

async fn exec(program: String, args: Vec<String>, timeout_secs: Option<u64>) -> Result<ExecResult, String> {
	let started = Instant::now();
	let mut cmd = Command::new(&program);
	cmd.args(&args)
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		// Kill the child if we stop waiting on it (the timeout below, or the task
		// being dropped). Without this a timed-out process is orphaned and keeps
		// running, since tokio does not kill children on drop by default.
		.kill_on_drop(true);
	// Don't flash a console window on Windows — the agent runs windowless, so a
	// spawned console program would otherwise pop one up on the guest's screen.
	crate::proc::NoWindow::no_window(&mut cmd);

	let mut child = cmd.spawn().map_err(|e| format!("spawning {program}: {e}"))?;
	let timeout = Duration::from_secs(
		timeout_secs
			.unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS)
			.clamp(1, MAX_EXEC_TIMEOUT_SECS),
	);

	// Drain stdout/stderr concurrently with the wait, retaining at most MAX_OUTPUT
	// per stream. `wait_with_output` buffers *all* output in memory, so a command
	// that emits gigabytes (`yes`, `cat /dev/urandom`) within the timeout window
	// would OOM the agent — and the oversized response would then exceed the frame
	// cap and fail to send anyway. We keep draining past the cap (discarding) so a
	// well-behaved command that simply prints a lot still exits promptly instead of
	// blocking on a full pipe.
	let stdout = child.stdout.take();
	let stderr = child.stderr.take();
	let collect = async {
		let out = async {
			match stdout {
				Some(s) => read_capped(s, MAX_OUTPUT).await,
				None => (Vec::new(), false),
			}
		};
		let err = async {
			match stderr {
				Some(s) => read_capped(s, MAX_OUTPUT).await,
				None => (Vec::new(), false),
			}
		};
		tokio::join!(out, err, child.wait())
	};

	let (stdout, stderr, status) = match tokio::time::timeout(timeout, collect).await {
		Ok(triple) => triple,
		Err(_) => return Err(format!("{program} timed out after {}s", timeout.as_secs())),
	};
	let status = status.map_err(|e| format!("running {program}: {e}"))?;

	Ok(ExecResult {
		code: status.code(),
		stdout: decode_output(stdout),
		stderr: decode_output(stderr),
		duration_ms: started.elapsed().as_millis() as u64,
	})
}

/// Cap on how much of each of a command's stdout/stderr the agent keeps in memory.
const MAX_OUTPUT: usize = 1024 * 1024;

/// Read `reader` to EOF, retaining at most `cap` bytes (discarding the rest so the
/// child never blocks on a full pipe). Returns the bytes and whether output was
/// dropped past the cap.
async fn read_capped<R>(mut reader: R, cap: usize) -> (Vec<u8>, bool)
where
	R: tokio::io::AsyncRead + Unpin,
{
	use tokio::io::AsyncReadExt;
	let mut buf = Vec::new();
	let mut chunk = [0u8; 8192];
	let mut truncated = false;
	loop {
		match reader.read(&mut chunk).await {
			Ok(0) | Err(_) => break,
			Ok(n) => {
				let room = cap.saturating_sub(buf.len());
				let take = room.min(n);
				buf.extend_from_slice(&chunk[..take]);
				if take < n {
					truncated = true;
				}
			}
		}
	}
	(buf, truncated)
}

/// Lossily decode captured bytes to a string, appending a marker when the stream
/// was truncated at [`MAX_OUTPUT`] so the operator knows output was cut.
fn decode_output((bytes, truncated): (Vec<u8>, bool)) -> String {
	let mut s = String::from_utf8_lossy(&bytes).into_owned();
	if truncated {
		s.push_str("\n…[output truncated at 1 MiB]");
	}
	s
}

async fn screenshot(display: u32) -> Result<ScreenshotResult, String> {
	// On Wayland, go through the Screenshot portal (the X11 grab would be black
	// for native Wayland windows).
	#[cfg(target_os = "linux")]
	if crate::platform::is_wayland() {
		let (png, width, height) = crate::wayland::screenshot().await.map_err(|e| e.to_string())?;
		return Ok(ScreenshotResult {
			display,
			width,
			height,
			png_base64: B64.encode(png),
		});
	}

	crate::x11env::ensure();
	tokio::task::spawn_blocking(move || {
		let cap = capture::capture(display).map_err(|e| e.to_string())?;
		let png = capture::encode_png(&cap.image).map_err(|e| e.to_string())?;
		Ok(ScreenshotResult {
			display,
			width: cap.width,
			height: cap.height,
			png_base64: B64.encode(png),
		})
	})
	.await
	.map_err(|e| format!("screenshot task failed: {e}"))?
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn read_capped_retains_up_to_the_cap_and_flags_truncation() {
		// More than the cap: retains exactly `cap` bytes and reports truncation.
		let data = [b'x'; 100];
		let (buf, truncated) = read_capped(&data[..], 10).await;
		assert_eq!(buf.len(), 10);
		assert!(truncated);
		// Under the cap: kept whole, not flagged.
		let (buf, truncated) = read_capped(&b"hello"[..], 10).await;
		assert_eq!(buf, b"hello");
		assert!(!truncated);
	}

	#[test]
	fn decode_output_appends_a_marker_only_when_truncated() {
		assert_eq!(decode_output((b"hi".to_vec(), false)), "hi");
		assert!(decode_output((b"hi".to_vec(), true)).contains("truncated"));
	}
}
