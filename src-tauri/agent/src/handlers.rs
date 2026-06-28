//! One-shot control-plane request handlers (status, exec, screenshot).

use std::process::Stdio;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use tether_protocol::{AgentStatus, ControlRequest, ControlResponse, ExecResult, ScreenshotResult};
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
	match req {
		ControlRequest::Ping => ControlResponse::Pong,
		ControlRequest::Status => ControlResponse::Status(status()),
		ControlRequest::Exec {
			program,
			args,
			timeout_secs,
		} => match exec(program, args, timeout_secs).await {
			Ok(r) => ControlResponse::Exec(r),
			Err(e) => ControlResponse::Error { message: e },
		},
		ControlRequest::Screenshot { display } => match screenshot(display.unwrap_or(0)).await {
			Ok(r) => ControlResponse::Screenshot(r),
			Err(e) => ControlResponse::Error { message: e },
		},
		ControlRequest::EnableRdp => match tokio::task::spawn_blocking(crate::rdp::enable).await {
			Ok(Ok(info)) => ControlResponse::Rdp(info),
			Ok(Err(e)) => ControlResponse::Error { message: e },
			Err(e) => ControlResponse::Error {
				message: format!("rdp task failed: {e}"),
			},
		},
	}
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
	}
}

async fn exec(program: String, args: Vec<String>, timeout_secs: Option<u64>) -> Result<ExecResult, String> {
	let started = Instant::now();
	let mut cmd = Command::new(&program);
	cmd.args(&args)
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped());

	let child = cmd.spawn().map_err(|e| format!("spawning {program}: {e}"))?;
	let timeout = Duration::from_secs(timeout_secs.unwrap_or(30).clamp(1, 600));

	let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
		Ok(Ok(out)) => out,
		Ok(Err(e)) => return Err(format!("running {program}: {e}")),
		Err(_) => return Err(format!("{program} timed out after {}s", timeout.as_secs())),
	};

	Ok(ExecResult {
		code: output.status.code(),
		stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
		stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
		duration_ms: started.elapsed().as_millis() as u64,
	})
}

async fn screenshot(display: u32) -> Result<ScreenshotResult, String> {
	// On Wayland, go through the Screenshot portal (the X11 grab would be black
	// for native Wayland windows).
	#[cfg(feature = "wayland")]
	if crate::platform::is_wayland() {
		let (png, width, height) = crate::wayland::screenshot().await.map_err(|e| e.to_string())?;
		return Ok(ScreenshotResult {
			display,
			width,
			height,
			png_base64: B64.encode(png),
		});
	}

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
