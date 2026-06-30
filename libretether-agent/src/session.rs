//! Live screen-control session: a single full-duplex QUIC stream carrying
//! captured frames out and input events in.
//!
//! There are two backends, chosen at runtime: an X11 path (`xcap` + `enigo`) and
//! a Wayland path that goes through the XDG desktop portals (see [`crate::wayland`]).
//!
//! Both backends share the same three-stage pipeline so capture, encode and
//! network write overlap instead of running serially:
//!
//! ```text
//! capture thread ──RawFrame──▶ encoder thread ──OutFrame──▶ async writer
//!   (xcap / pw)   single-slot   (tile delta +    bounded     (QUIC stream)
//!                 newest-wins    parallel JPEG)
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TrySendError;
use std::sync::Arc;
use std::time::Duration;

use libretether_protocol::frame::{read_frame_capped, MAX_CONTROL_FRAME};
use libretether_protocol::video::{self};
use libretether_protocol::{SessionClient, SessionConfig, SessionServer};
use quinn::{RecvStream, SendStream};

use crate::capture;
use crate::encode::{self, OutFrame, RawFrame, SharedConfig};
use crate::input::{self, InjectCmd};

/// Run a session to completion. The opening [`SessionClient::Start`] is read
/// here, then the call is dispatched to the right backend for this session.
pub async fn run(mut send: SendStream, mut recv: RecvStream) -> std::io::Result<()> {
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

	#[cfg(target_os = "linux")]
	if crate::platform::is_wayland() {
		return crate::wayland::run_session(cfg, send, recv).await;
	}

	x11_session(cfg, send, recv).await
}

/// X11 backend: stateless per-frame `xcap` capture + `enigo` input injection.
async fn x11_session(cfg: SessionConfig, mut send: SendStream, mut recv: RecvStream) -> std::io::Result<()> {
	// Recover DISPLAY/XAUTHORITY from the live session before the capture and
	// injector threads start, so both can authenticate to the X server.
	crate::x11env::ensure();

	let stop = Arc::new(AtomicBool::new(false));
	let shared = SharedConfig::new(&cfg);
	let (injector, injector_thread) = input::spawn();

	// Single-slot capture→encode hop (newest wins) and a small encode→write hop.
	let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<RawFrame>(1);
	let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<OutFrame>(2);
	let capture = spawn_capture(cfg.display, shared.clone(), stop.clone(), raw_tx);
	let encoder = {
		let shared = shared.clone();
		std::thread::spawn(move || encode::run(raw_rx, out_tx, shared))
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
	let _ = send.finish();
	reader.abort();
	let _ = injector.send(InjectCmd::Stop);
	let _ = injector_thread.join();
	let _ = capture.join();
	let _ = encoder.join();
	Ok(())
}

/// Capture thread: grab a full RGBA frame at the configured rate and hand it to
/// the encoder, dropping it if the encoder is still busy (stay real-time).
fn spawn_capture(
	display: u32,
	shared: Arc<SharedConfig>,
	stop: Arc<AtomicBool>,
	tx: std::sync::mpsc::SyncSender<RawFrame>,
) -> std::thread::JoinHandle<()> {
	std::thread::spawn(move || {
		while !stop.load(Ordering::Relaxed) {
			let started = std::time::Instant::now();
			let interval = Duration::from_millis(1000 / shared.max_fps());
			match capture::capture(display) {
				Ok(cap) => {
					let raw = RawFrame {
						width: cap.width,
						height: cap.height,
						origin_x: cap.origin_x,
						origin_y: cap.origin_y,
						rgba: cap.image,
					};
					match tx.try_send(raw) {
						// Encoder busy — drop this frame, the next capture is fresher.
						Ok(()) | Err(TrySendError::Full(_)) => {}
						Err(TrySendError::Disconnected(_)) => break,
					}
				}
				Err(e) => {
					crate::net::log(&format!("capture error: {e}"));
					std::thread::sleep(Duration::from_millis(500));
				}
			}
			if let Some(rem) = interval.checked_sub(started.elapsed()) {
				std::thread::sleep(rem);
			}
		}
	})
}
