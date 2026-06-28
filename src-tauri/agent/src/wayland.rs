//! Wayland backend via the XDG desktop portals.
//!
//! Capture goes through the ScreenCast portal (a PipeWire stream); input goes
//! through the RemoteDesktop portal. Both are negotiated on a single session, so
//! the user sees one "share your screen" consent prompt and absolute pointer
//! motion can reference the ScreenCast stream's node id.

use anyhow::{anyhow, Result};
use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};
use ashpd::enumflags2::BitFlags;
use quinn::{RecvStream, SendStream};
use tether_protocol::frame::{read_frame, write_frame};
use tether_protocol::{InputEvent, MouseButton, SessionClient, SessionConfig, SessionServer};

// Linux evdev button codes (see <linux/input-event-codes.h>).
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;

/// A negotiated portal session: input control + a screen-cast stream.
struct Portal {
	rd: RemoteDesktop,
	#[cfg_attr(not(feature = "wayland-capture"), allow(dead_code))]
	screencast: Screencast,
	session: Session<RemoteDesktop>,
	node_id: u32,
	width: u32,
	height: u32,
}

/// Entry point used by `session::run` when on Wayland.
pub async fn run_session(cfg: SessionConfig, send: SendStream, recv: RecvStream) -> std::io::Result<()> {
	if let Err(e) = serve(cfg, send, recv).await {
		eprintln!("[tether-agent] wayland session: {e:#}");
	}
	Ok(())
}

async fn serve(cfg: SessionConfig, mut send: SendStream, recv: RecvStream) -> Result<()> {
	let portal = match setup_portal().await {
		Ok(p) => p,
		Err(e) => {
			let _ = write_frame(
				&mut send,
				&SessionServer::Error {
					message: format!("portal setup failed: {e}"),
				},
			)
			.await;
			return Ok(());
		}
	};

	let _ = write_frame(
		&mut send,
		&SessionServer::Meta {
			display: cfg.display,
			width: portal.width,
			height: portal.height,
		},
	)
	.await;

	// Live capture (feature-gated — needs libpipewire).
	#[cfg(feature = "wayland-capture")]
	let (stop, mut frame_rx) = {
		use std::sync::atomic::AtomicBool;
		use std::sync::Arc;
		let fd = portal
			.screencast
			.open_pipe_wire_remote(&portal.session, Default::default())
			.await?;
		let stop = Arc::new(AtomicBool::new(false));
		let (tx, rx) = tokio::sync::mpsc::channel::<crate::session::Encoded>(4);
		crate::pwstream::spawn(fd, portal.node_id, cfg.quality, cfg.max_fps, stop.clone(), tx);
		(stop, rx)
	};
	#[cfg(not(feature = "wayland-capture"))]
	let _ = write_frame(
		&mut send,
		&SessionServer::Error {
			message: "agent built without `wayland-capture` — input works but live frames are off".into(),
		},
	)
	.await;

	// Read input on a dedicated task so the framed read is never cancelled
	// mid-message by the writer's select! loop.
	let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
	let reader = tokio::spawn(async move {
		let mut recv = recv;
		loop {
			match read_frame::<_, SessionClient>(&mut recv).await {
				Ok(SessionClient::Input(ev)) => {
					if input_tx.send(ev).is_err() {
						break;
					}
				}
				Ok(SessionClient::Refresh) | Ok(SessionClient::Start(_)) => {}
				Ok(SessionClient::Stop) | Err(_) => break,
			}
		}
	});

	// The no-capture build collapses this to a single-arm receive; the capture
	// build needs the `loop`/`select!` form, so keep the loop either way.
	#[allow(clippy::while_let_loop)]
	loop {
		#[cfg(feature = "wayland-capture")]
		tokio::select! {
			ev = input_rx.recv() => match ev {
				Some(ev) => inject(&portal, ev).await,
				None => break,
			},
			frame = frame_rx.recv() => match frame {
				Some(enc) => {
					if write_frame(&mut send, &crate::session::to_frame(&enc)).await.is_err() {
						break;
					}
				}
				None => break,
			},
		}

		#[cfg(not(feature = "wayland-capture"))]
		match input_rx.recv().await {
			Some(ev) => inject(&portal, ev).await,
			None => break,
		}
	}

	#[cfg(feature = "wayland-capture")]
	stop.store(true, std::sync::atomic::Ordering::Relaxed);
	reader.abort();
	let _ = portal.session.close().await;
	let _ = send.finish();
	Ok(())
}

async fn setup_portal() -> Result<Portal> {
	let rd = RemoteDesktop::new().await?;
	let screencast = Screencast::new().await?;
	let session = rd.create_session(Default::default()).await?;

	rd.select_devices(
		&session,
		SelectDevicesOptions::default().set_devices(DeviceType::Keyboard | DeviceType::Pointer),
	)
	.await?;
	// Persist the grant with a restore token so the consent dialog only appears
	// the first time — later connects reuse the saved token silently.
	let saved_token = load_restore_token();
	screencast
		.select_sources(
			&session,
			SelectSourcesOptions::default()
				.set_cursor_mode(CursorMode::Embedded)
				.set_sources(BitFlags::from(SourceType::Monitor))
				.set_multiple(false)
				.set_persist_mode(PersistMode::ExplicitlyRevoked)
				.set_restore_token(saved_token.as_deref()),
		)
		.await?;

	let response = rd.start(&session, None, Default::default()).await?.response()?;
	if let Some(token) = response.restore_token() {
		save_restore_token(token);
	}
	let stream = response
		.streams()
		.first()
		.ok_or_else(|| anyhow!("portal returned no screen stream"))?;
	let node_id = stream.pipe_wire_node_id();
	let (w, h) = stream.size().unwrap_or((1920, 1080));

	Ok(Portal {
		rd,
		screencast,
		session,
		node_id,
		width: w as u32,
		height: h as u32,
	})
}

fn restore_token_path() -> Option<std::path::PathBuf> {
	Some(dirs::config_dir()?.join("tether-agent").join("wayland.token"))
}

/// Load the saved ScreenCast restore token, if any.
fn load_restore_token() -> Option<String> {
	let path = restore_token_path()?;
	std::fs::read_to_string(path)
		.ok()
		.map(|s| s.trim().to_string())
		.filter(|s| !s.is_empty())
}

/// Persist the ScreenCast restore token returned by the portal.
fn save_restore_token(token: &str) {
	if let Some(path) = restore_token_path() {
		if let Some(dir) = path.parent() {
			let _ = std::fs::create_dir_all(dir);
		}
		let _ = std::fs::write(path, token);
	}
}

async fn inject(portal: &Portal, ev: InputEvent) {
	let session = &portal.session;
	let rd = &portal.rd;
	let _ = match ev {
		InputEvent::MouseMove { x, y } => {
			let px = x.clamp(0.0, 1.0) * portal.width as f64;
			let py = y.clamp(0.0, 1.0) * portal.height as f64;
			rd.notify_pointer_motion_absolute(session, portal.node_id, px, py, Default::default())
				.await
		}
		InputEvent::MouseButton { button, pressed } => {
			rd.notify_pointer_button(session, evdev_button(button), key_state(pressed), Default::default())
				.await
		}
		InputEvent::MouseScroll { dx, dy } => {
			if dy != 0 {
				let _ = rd
					.notify_pointer_axis_discrete(session, Axis::Vertical, dy, Default::default())
					.await;
			}
			if dx != 0 {
				let _ = rd
					.notify_pointer_axis_discrete(session, Axis::Horizontal, dx, Default::default())
					.await;
			}
			Ok(())
		}
		InputEvent::Key { code, pressed } => match evdev_keycode(&code) {
			Some(kc) => {
				rd.notify_keyboard_keycode(session, kc, key_state(pressed), Default::default())
					.await
			}
			None => Ok(()),
		},
		InputEvent::Text { text } => {
			for ch in text.chars() {
				let keysym = 0x0100_0000 + ch as i32;
				let _ = rd
					.notify_keyboard_keysym(session, keysym, KeyState::Pressed, Default::default())
					.await;
				let _ = rd
					.notify_keyboard_keysym(session, keysym, KeyState::Released, Default::default())
					.await;
			}
			Ok(())
		}
	};
}

fn key_state(pressed: bool) -> KeyState {
	if pressed {
		KeyState::Pressed
	} else {
		KeyState::Released
	}
}

fn evdev_button(b: MouseButton) -> i32 {
	match b {
		MouseButton::Left => BTN_LEFT,
		MouseButton::Right => BTN_RIGHT,
		MouseButton::Middle => BTN_MIDDLE,
	}
}

/// One-shot screenshot via the Screenshot portal (no PipeWire needed). Returns
/// `(png_bytes, width, height)`.
pub async fn screenshot() -> Result<(Vec<u8>, u32, u32)> {
	use ashpd::desktop::screenshot::Screenshot;

	let response = Screenshot::request()
		.interactive(false)
		.modal(false)
		.send()
		.await?
		.response()?;
	let uri = response.uri().as_str();
	let path = uri.strip_prefix("file://").unwrap_or(uri);
	let bytes = std::fs::read(path)?;
	let (width, height) = image::load_from_memory(&bytes)
		.map(|img| (img.width(), img.height()))
		.unwrap_or((0, 0));
	let _ = std::fs::remove_file(path);
	Ok((bytes, width, height))
}

/// Map a W3C `KeyboardEvent.code` to a Linux evdev keycode.
fn evdev_keycode(code: &str) -> Option<i32> {
	let kc = match code {
		"Escape" => 1,
		"Digit1" => 2,
		"Digit2" => 3,
		"Digit3" => 4,
		"Digit4" => 5,
		"Digit5" => 6,
		"Digit6" => 7,
		"Digit7" => 8,
		"Digit8" => 9,
		"Digit9" => 10,
		"Digit0" => 11,
		"Minus" => 12,
		"Equal" => 13,
		"Backspace" => 14,
		"Tab" => 15,
		"KeyQ" => 16,
		"KeyW" => 17,
		"KeyE" => 18,
		"KeyR" => 19,
		"KeyT" => 20,
		"KeyY" => 21,
		"KeyU" => 22,
		"KeyI" => 23,
		"KeyO" => 24,
		"KeyP" => 25,
		"BracketLeft" => 26,
		"BracketRight" => 27,
		"Enter" | "NumpadEnter" => 28,
		"ControlLeft" => 29,
		"KeyA" => 30,
		"KeyS" => 31,
		"KeyD" => 32,
		"KeyF" => 33,
		"KeyG" => 34,
		"KeyH" => 35,
		"KeyJ" => 36,
		"KeyK" => 37,
		"KeyL" => 38,
		"Semicolon" => 39,
		"Quote" => 40,
		"Backquote" => 41,
		"ShiftLeft" => 42,
		"Backslash" => 43,
		"KeyZ" => 44,
		"KeyX" => 45,
		"KeyC" => 46,
		"KeyV" => 47,
		"KeyB" => 48,
		"KeyN" => 49,
		"KeyM" => 50,
		"Comma" => 51,
		"Period" => 52,
		"Slash" => 53,
		"ShiftRight" => 54,
		"AltLeft" => 56,
		"Space" => 57,
		"CapsLock" => 58,
		"F1" => 59,
		"F2" => 60,
		"F3" => 61,
		"F4" => 62,
		"F5" => 63,
		"F6" => 64,
		"F7" => 65,
		"F8" => 66,
		"F9" => 67,
		"F10" => 68,
		"F11" => 87,
		"F12" => 88,
		"ControlRight" => 97,
		"AltRight" => 100,
		"Home" => 102,
		"ArrowUp" => 103,
		"PageUp" => 104,
		"ArrowLeft" => 105,
		"ArrowRight" => 106,
		"End" => 107,
		"ArrowDown" => 108,
		"PageDown" => 109,
		"Insert" => 110,
		"Delete" => 111,
		"MetaLeft" => 125,
		"MetaRight" => 126,
		_ => return None,
	};
	Some(kc)
}
