//! Live screen-control session: a single full-duplex QUIC stream carrying
//! captured frames out and input events in.
//!
//! There are two backends, chosen at runtime: an X11 path (`xcap` + `enigo`) and
//! a Wayland path that goes through the XDG desktop portals (see [`crate::wayland`]).
//!
//! All backends share the same three-stage pipeline so capture, encode and
//! network write overlap instead of running serially:
//!
//! ```text
//! capture thread ──RawFrame──▶ encoder thread ──OutFrame──▶ async writer
//!  (DXGI/xcap/pw) single-slot   (tile delta +    bounded     (QUIC stream)
//!                 newest-wins    parallel JPEG)
//! ```
//!
//! The capture stage is per-platform: DXGI Desktop Duplication on Windows
//! ([`crate::wincap`]), a PipeWire portal stream on Wayland ([`crate::pwstream`]),
//! and an `xcap` poll loop elsewhere (X11/macOS).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use libretether_protocol::e2e::{SecureQuicRecv, SecureQuicSend};
use libretether_protocol::frame::{read_frame_capped, MAX_CONTROL_FRAME};
use libretether_protocol::video::{self};
use libretether_protocol::{SessionClient, SessionConfig, SessionServer};
use tokio::io::AsyncWriteExt;

use crate::encode::{self, OutFrame, RawFrame, SharedConfig};
use crate::input::{self, InjectCmd};

/// Run a session to completion. The opening [`SessionClient::Start`] is read
/// here, then the call is dispatched to the right backend for this session. The
/// streams are the end-to-end-encrypted halves (see [`crate::net`]); everything
/// written here is AEAD-sealed before it reaches the wire.
pub async fn run(mut send: SecureQuicSend, mut recv: SecureQuicRecv) -> std::io::Result<()> {
	// Input/control events are small — cap the read tightly so a buggy or hostile
	// controller can't force a large allocation on the session stream (frames the
	// other direction legitimately need the wide cap; these never do).
	let cfg = match read_frame_capped::<_, SessionClient>(&mut recv, MAX_CONTROL_FRAME).await? {
		SessionClient::Start(cfg) => cfg.sanitized(),
		_ => {
			let _ = video::write_control(
				&mut send,
				&SessionServer::Error {
					message: "expected start".into(),
				},
			)
			.await;
			return Ok(());
		}
	};
	crate::net::log(&format!(
		"session starting: display {} {}kbps {}fps scale {}%{}",
		cfg.display,
		cfg.bitrate_kbps,
		cfg.max_fps,
		cfg.scale,
		if cfg.auto { " (auto)" } else { "" },
	));

	#[cfg(target_os = "linux")]
	if crate::platform::is_wayland() {
		crate::net::debug("session backend: linux/wayland (portals + pipewire)");
		let result = crate::wayland::run_session(cfg, send, recv).await;
		crate::net::log("session ended");
		return result;
	}

	// The non-Wayland path dispatches capture per-OS: X11 via xcap on Linux, DXGI
	// Desktop Duplication on Windows (GDI fallback logged separately by `wincap`),
	// CoreGraphics via xcap on macOS. The label reflects that, not the shared
	// `x11_session` driver's name.
	#[cfg(target_os = "windows")]
	crate::net::debug("session backend: windows/dxgi");
	#[cfg(target_os = "macos")]
	crate::net::debug("session backend: macos/xcap");
	#[cfg(not(any(target_os = "windows", target_os = "macos")))]
	crate::net::debug("session backend: linux/x11+xcap");
	let result = x11_session(cfg, send, recv).await;
	crate::net::log("session ended");
	result
}

/// X11 backend: stateless per-frame `xcap` capture + `enigo` input injection.
async fn x11_session(cfg: SessionConfig, mut send: SecureQuicSend, mut recv: SecureQuicRecv) -> std::io::Result<()> {
	// Recover DISPLAY/XAUTHORITY from the live session before the capture and
	// injector threads start, so both can authenticate to the X server.
	crate::x11env::ensure();

	let stop = Arc::new(AtomicBool::new(false));
	let shared = SharedConfig::new(&cfg);
	let (injector, injector_thread) = input::spawn();

	// Single-slot capture→encode hop (newest wins) and a small encode→write hop.
	let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<RawFrame>(1);
	let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<OutFrame>(2);

	// Windows GPU zero-copy path (opt-in via LIBRETETHER_ENCODER=gpu): DXGI → GPU
	// BGRA→NV12 → Media Foundation encode, all on one device, emitting OutFrames
	// directly. It owns capture *and* encode; falls back to the CPU pipeline below if
	// it isn't requested or can't be set up.
	#[cfg(target_os = "windows")]
	let hw = if crate::wincap_hw::requested() {
		crate::wincap_hw::try_spawn(cfg.display, shared.clone(), stop.clone(), out_tx.clone())
	} else {
		None
	};
	#[cfg(not(target_os = "windows"))]
	let hw: Option<std::thread::JoinHandle<()>> = None;

	// The CPU pipeline (capture thread + encode thread) — used unless the GPU path took
	// over. `capture`/`encoder` are `None` in that case.
	let (capture, encoder) = if hw.is_some() {
		(None, None)
	} else {
		let capture = spawn_capture(cfg.display, shared.clone(), stop.clone(), raw_tx);
		let encoder = {
			let shared = shared.clone();
			Some(std::thread::spawn(move || encode::run(raw_rx, out_tx, shared)))
		};
		(Some(capture), encoder)
	};

	let reader = {
		let stop = stop.clone();
		let injector = injector.clone();
		let shared = shared.clone();
		tokio::spawn(async move {
			loop {
				match read_frame_capped::<_, SessionClient>(&mut recv, MAX_CONTROL_FRAME).await {
					Ok(SessionClient::Input(ev)) => {
						let _ = injector.send(InjectCmd::Event(ev));
					}
					Ok(SessionClient::Configure(cfg)) => shared.apply(&cfg.sanitized()),
					Ok(SessionClient::Refresh) => shared.request_keyframe(),
					Ok(SessionClient::Start(_)) => {}
					Ok(SessionClient::Stop) => break,
					// A single undecodable frame should drop that event, not the
					// whole session — only a real stream error ends the loop.
					Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
						crate::net::log(&format!("ignoring malformed session frame: {e}"));
					}
					Err(_) => break,
				}
			}
			stop.store(true, Ordering::Relaxed);
		})
	};

	let mut last_geom = (0u32, 0u32, 0i32, 0i32);
	while let Some(out) = out_rx.recv().await {
		let geom = (out.source_width, out.source_height, out.origin_x, out.origin_y);
		if geom != last_geom {
			last_geom = geom;
			let _ = injector.send(InjectCmd::Geometry {
				width: out.source_width,
				height: out.source_height,
				origin_x: out.origin_x,
				origin_y: out.origin_y,
			});
			if video::write_control(
				&mut send,
				&SessionServer::Meta {
					display: cfg.display,
					width: out.source_width,
					height: out.source_height,
					capture: shared.capture_backend().to_string(),
					encoder: shared.encoder_backend().to_string(),
				},
			)
			.await
			.is_err()
			{
				break;
			}
		}
		if video::write_message(&mut send, &out.body).await.is_err() {
			break;
		}
	}

	// Wind the workers down deterministically: signal stop, drop the encode→write
	// receiver so the encoder's `blocking_send` unblocks, then stop + join the
	// injector and join the capture/encode threads so none outlives the session (a
	// lingering capture loop would contend with the next session for `xcap`).
	stop.store(true, Ordering::Relaxed);
	drop(out_rx);
	let _ = send.shutdown().await;
	reader.abort();
	let _ = injector.send(InjectCmd::Stop);
	let _ = injector_thread.join();
	if let Some(capture) = capture {
		let _ = capture.join();
	}
	if let Some(encoder) = encoder {
		let _ = encoder.join();
	}
	if let Some(hw) = hw {
		let _ = hw.join();
	}
	Ok(())
}

/// Windows capture: DXGI Desktop Duplication — persistent, GPU-fast, event-driven
/// (see [`crate::wincap`]).
#[cfg(target_os = "windows")]
fn spawn_capture(
	display: u32,
	shared: Arc<SharedConfig>,
	stop: Arc<AtomicBool>,
	tx: std::sync::mpsc::SyncSender<RawFrame>,
) -> std::thread::JoinHandle<()> {
	crate::wincap::spawn(display, shared, stop, tx)
}

/// Capture thread (X11/macOS): the shared `xcap` poll loop.
#[cfg(not(target_os = "windows"))]
fn spawn_capture(
	display: u32,
	shared: Arc<SharedConfig>,
	stop: Arc<AtomicBool>,
	tx: std::sync::mpsc::SyncSender<RawFrame>,
) -> std::thread::JoinHandle<()> {
	shared.report_capture("xcap");
	std::thread::spawn(move || crate::capture::poll_loop(display, shared, stop, tx))
}
