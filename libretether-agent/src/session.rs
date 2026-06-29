//! Live screen-control session: a single full-duplex QUIC stream carrying
//! captured frames out and input events in.
//!
//! There are two backends, chosen at runtime: an X11 path (`xcap` + `enigo`) and
//! a Wayland path that goes through the XDG desktop portals (see [`crate::wayland`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use libretether_protocol::frame::{read_frame, write_frame};
use libretether_protocol::{Frame, FrameEncoding, SessionClient, SessionConfig, SessionServer};
use quinn::{RecvStream, SendStream};

use crate::capture;
use crate::input::{self, InjectCmd};

/// One captured + encoded frame handed from a blocking capture thread to the
/// async writer.
pub(crate) struct Encoded {
	pub seq: u64,
	pub width: u32,
	pub height: u32,
	pub jpeg: Vec<u8>,
}

/// Run a session to completion. The opening [`SessionClient::Start`] is read
/// here, then the call is dispatched to the right backend for this session.
pub async fn run(mut send: SendStream, mut recv: RecvStream) -> std::io::Result<()> {
	let cfg = match read_frame::<_, SessionClient>(&mut recv).await? {
		SessionClient::Start(cfg) => cfg,
		_ => {
			let _ = write_frame(
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
	let injector = input::spawn();

	let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Encoded>(4);
	spawn_capture(cfg.clone(), stop.clone(), frame_tx);

	let reader = {
		let stop = stop.clone();
		let injector = injector.clone();
		tokio::spawn(async move {
			loop {
				match read_frame::<_, SessionClient>(&mut recv).await {
					Ok(SessionClient::Input(ev)) => {
						let _ = injector.send(InjectCmd::Event(ev));
					}
					Ok(SessionClient::Refresh) | Ok(SessionClient::Start(_)) => {}
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

	let mut last_geom = (0u32, 0u32);
	while let Some(enc) = frame_rx.recv().await {
		if (enc.width, enc.height) != last_geom {
			last_geom = (enc.width, enc.height);
			let _ = injector.send(InjectCmd::Geometry {
				width: enc.width,
				height: enc.height,
			});
			write_frame(
				&mut send,
				&SessionServer::Meta {
					display: cfg.display,
					width: enc.width,
					height: enc.height,
				},
			)
			.await?;
		}
		if write_frame(&mut send, &to_frame(&enc)).await.is_err() {
			break;
		}
	}

	stop.store(true, Ordering::Relaxed);
	let _ = injector.send(InjectCmd::Stop);
	let _ = send.finish();
	reader.abort();
	Ok(())
}

pub(crate) fn to_frame(enc: &Encoded) -> SessionServer {
	SessionServer::Frame(Frame {
		seq: enc.seq,
		width: enc.width,
		height: enc.height,
		encoding: FrameEncoding::Jpeg,
		data_base64: B64.encode(&enc.jpeg),
	})
}

fn spawn_capture(cfg: SessionConfig, stop: Arc<AtomicBool>, tx: tokio::sync::mpsc::Sender<Encoded>) {
	std::thread::spawn(move || {
		let fps = cfg.max_fps.clamp(1, 60) as u64;
		let interval = Duration::from_millis(1000 / fps);
		let mut seq = 0u64;
		while !stop.load(Ordering::Relaxed) {
			let started = std::time::Instant::now();
			match capture::capture(cfg.display) {
				Ok(cap) => match capture::encode_jpeg(&cap.image, cfg.quality) {
					Ok(jpeg) => {
						seq += 1;
						let enc = Encoded {
							seq,
							width: cap.width,
							height: cap.height,
							jpeg,
						};
						if tx.blocking_send(enc).is_err() {
							break;
						}
					}
					Err(e) => eprintln!("[libretether-agent] encode error: {e}"),
				},
				Err(e) => {
					eprintln!("[libretether-agent] capture error: {e}");
					std::thread::sleep(Duration::from_millis(500));
				}
			}
			if let Some(rem) = interval.checked_sub(started.elapsed()) {
				std::thread::sleep(rem);
			}
		}
	});
}
