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
use libretether_protocol::e2e::{SecureQuicRecv, SecureQuicSend};
use libretether_protocol::frame::{read_frame_capped, MAX_CONTROL_FRAME};
use libretether_protocol::video;
use libretether_protocol::{InputEvent, MouseButton, SessionClient, SessionConfig, SessionServer};
use tokio::io::AsyncWriteExt;

use crate::encode::{self, OutFrame, RawFrame, SharedConfig};

// Linux evdev button codes (see <linux/input-event-codes.h>).
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;

/// A negotiated portal session: input control + a screen-cast stream.
struct Portal {
	rd: RemoteDesktop,
	screencast: Screencast,
	session: Session<RemoteDesktop>,
	node_id: u32,
	width: u32,
	height: u32,
}

/// Entry point used by `session::run` when on Wayland.
pub async fn run_session(cfg: SessionConfig, send: SecureQuicSend, recv: SecureQuicRecv) -> std::io::Result<()> {
	if let Err(e) = serve(cfg, send, recv).await {
		crate::net::log(&format!("wayland session: {e:#}"));
	}
	Ok(())
}

async fn serve(cfg: SessionConfig, mut send: SecureQuicSend, recv: SecureQuicRecv) -> Result<()> {
	let cfg = cfg.sanitized();
	let portal = match setup_portal().await {
		Ok(p) => p,
		Err(e) => {
			let _ = video::write_control(
				&mut send,
				&SessionServer::Error {
					message: format!("portal setup failed: {e}"),
				},
			)
			.await;
			return Ok(());
		}
	};

	// Start the PipeWire capture thread feeding the shared encoder. Meta is sent
	// from the frame loop on the first frame (not up front like the geometry-only
	// version was) so it can carry the encoder backend, which isn't known until the
	// encode thread has built it.
	let fd = portal
		.screencast
		.open_pipe_wire_remote(&portal.session, Default::default())
		.await?;
	let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
	let shared = SharedConfig::new(&cfg);
	shared.report_capture("PipeWire");
	let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<RawFrame>(1);
	let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<OutFrame>(2);
	crate::pwstream::spawn(fd, portal.node_id, shared.clone(), stop.clone(), raw_tx);
	let encoder = {
		let shared = shared.clone();
		std::thread::spawn(move || encode::run(raw_rx, out_tx, shared))
	};

	// Read input on its own task (so a framed read is never cancelled mid-message)
	// and inject on another. Injection must NOT share the frame-writing loop: a
	// portal call can be slow or block (e.g. a burst of pointer motion, each a
	// D-Bus round-trip), and coupling the two would stall frame delivery and tear
	// the session down. The frame loop below owns the session's lifetime; input
	// failures only stop input, never the video.
	let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
	let reader = {
		let shared = shared.clone();
		tokio::spawn(async move {
			let mut recv = recv;
			loop {
				// Input/control events are small — cap the read tightly (frames flow the
				// other direction; these never need the wide cap).
				match read_frame_capped::<_, SessionClient>(&mut recv, MAX_CONTROL_FRAME).await {
					Ok(SessionClient::Input(ev)) => {
						if input_tx.send(ev).is_err() {
							break;
						}
					}
					Ok(SessionClient::Configure(cfg)) => shared.apply(&cfg.sanitized()),
					Ok(SessionClient::Refresh) => shared.request_keyframe(),
					Ok(SessionClient::Start(_)) => {}
					Ok(SessionClient::Stop) => break,
					// Drop a single undecodable frame rather than ending input.
					Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
						crate::net::log(&format!("ignoring malformed session frame: {e}"));
					}
					Err(_) => break,
				}
			}
		})
	};

	let portal = std::sync::Arc::new(portal);
	let injector = {
		let portal = portal.clone();
		tokio::spawn(async move {
			let mut pressed = crate::input::Pressed::default();
			while let Some(ev) = input_rx.recv().await {
				pressed.track(&ev);
				inject(&portal, ev).await;
			}
			// The input channel closed (session ending) — release anything still held
			// so a modifier/button down at teardown doesn't stay stuck on the remote.
			release_all(&portal, &pressed).await;
		})
	};

	// Frame loop: forward encoded frames until capture ends or the controller
	// disconnects. Input is handled independently above. Meta goes out on the first
	// frame, once both the capture and encoder backends have reported themselves.
	let mut meta_sent = false;
	while let Some(out) = out_rx.recv().await {
		if !meta_sent {
			meta_sent = true;
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

	stop.store(true, std::sync::atomic::Ordering::Relaxed);
	// Drop the encode→write receiver so the encoder's `blocking_send` unblocks, then
	// join it. Abort the reader (drops its `input_tx`), which ends the injector loop
	// and lets it release any still-held keys/buttons before the portal closes. Bound
	// the wait so a wedged portal call can't hang teardown.
	drop(out_rx);
	let _ = encoder.join();
	reader.abort();
	let _ = tokio::time::timeout(std::time::Duration::from_secs(2), injector).await;
	let _ = portal.session.close().await;
	let _ = send.shutdown().await;
	Ok(())
}

/// Inject a release through the portal for every key/button still held — the
/// Wayland counterpart to the X11 injector's release-on-teardown.
async fn release_all(portal: &Portal, pressed: &crate::input::Pressed) {
	for code in &pressed.keys {
		if let Some(kc) = evdev_keycode(code) {
			let _ = portal
				.rd
				.notify_keyboard_keycode(&portal.session, kc, KeyState::Released, Default::default())
				.await;
		}
	}
	for &button in &pressed.buttons {
		let _ = portal
			.rd
			.notify_pointer_button(
				&portal.session,
				evdev_button(button),
				KeyState::Released,
				Default::default(),
			)
			.await;
	}
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
	// NOTE: GNOME's RemoteDesktop portal rejects persistence ("Remote desktop
	// sessions cannot persist"), so we cannot use a restore token here — the
	// consent dialog appears on every connect. For prompt-free unattended
	// control, run an X11 session on the client instead of Wayland.
	screencast
		.select_sources(
			&session,
			SelectSourcesOptions::default()
				.set_cursor_mode(CursorMode::Embedded)
				.set_sources(BitFlags::from(SourceType::Monitor))
				.set_multiple(false)
				.set_persist_mode(PersistMode::DoNot),
		)
		.await?;

	let response = rd.start(&session, None, Default::default()).await?.response()?;
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

async fn inject(portal: &Portal, ev: InputEvent) {
	let session = &portal.session;
	let rd = &portal.rd;
	let result = match ev {
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
	if let Err(e) = result {
		crate::net::log(&format!("portal input injection failed: {e}"));
	}
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
	// The portal returns a percent-encoded `file://` URI, so a path with a space or
	// other reserved byte (e.g. `/tmp/a%20b.png`) must be decoded before we read it.
	let path = percent_decode(uri.strip_prefix("file://").unwrap_or(uri));
	let bytes = std::fs::read(&path)?;
	let (width, height) = image::load_from_memory(&bytes)
		.map(|img| (img.width(), img.height()))
		.unwrap_or((0, 0));
	let _ = std::fs::remove_file(&path);
	Ok((bytes, width, height))
}

/// Decode `%XX` escapes in a URI path. Leaves a malformed or trailing `%` as-is.
fn percent_decode(s: &str) -> String {
	let bytes = s.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		match (
			bytes[i],
			bytes.get(i + 1).and_then(hex_val),
			bytes.get(i + 2).and_then(hex_val),
		) {
			(b'%', Some(hi), Some(lo)) => {
				out.push(hi << 4 | lo);
				i += 3;
			}
			(b, _, _) => {
				out.push(b);
				i += 1;
			}
		}
	}
	String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: &u8) -> Option<u8> {
	match b {
		b'0'..=b'9' => Some(b - b'0'),
		b'a'..=b'f' => Some(b - b'a' + 10),
		b'A'..=b'F' => Some(b - b'A' + 10),
		_ => None,
	}
}

/// Map a W3C `KeyboardEvent.code` to a Linux evdev keycode. Kept in lock-step
/// with the X11 backend's `crate::input::map_key` (see the parity test below).
pub(crate) fn evdev_keycode(code: &str) -> Option<i32> {
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn maps_known_codes_to_evdev_keycodes() {
		assert_eq!(evdev_keycode("Escape"), Some(1));
		assert_eq!(evdev_keycode("KeyA"), Some(30));
		assert_eq!(evdev_keycode("Enter"), Some(28));
		assert_eq!(evdev_keycode("NumpadEnter"), Some(28));
		assert_eq!(evdev_keycode("Space"), Some(57));
		assert_eq!(evdev_keycode("F1"), Some(59));
		assert_eq!(evdev_keycode("F12"), Some(88));
		assert_eq!(evdev_keycode("ArrowUp"), Some(103));
	}

	#[test]
	fn unknown_codes_map_to_none() {
		assert_eq!(evdev_keycode("Bogus"), None);
		assert_eq!(evdev_keycode(""), None);
		assert_eq!(evdev_keycode("KeyÆ"), None);
	}

	#[test]
	fn maps_evdev_buttons() {
		assert_eq!(evdev_button(MouseButton::Left), BTN_LEFT);
		assert_eq!(evdev_button(MouseButton::Right), BTN_RIGHT);
		assert_eq!(evdev_button(MouseButton::Middle), BTN_MIDDLE);
	}

	/// Every W3C `KeyboardEvent.code` the controller can send must map on *both*
	/// backends — otherwise a key works on X11 but silently does nothing on Wayland
	/// (or vice versa). This is the guard that caught `Insert` missing from the X11
	/// table; it fails the moment the two tables drift apart.
	#[test]
	fn x11_and_wayland_keymaps_agree_on_every_canonical_code() {
		use crate::input::map_key;

		let mut codes: Vec<String> = Vec::new();
		codes.extend((b'A'..=b'Z').map(|c| format!("Key{}", c as char)));
		codes.extend((0..=9).map(|d| format!("Digit{d}")));
		codes.extend((1..=12).map(|f| format!("F{f}")));
		codes.extend(
			[
				"Enter",
				"NumpadEnter",
				"Tab",
				"Space",
				"Backspace",
				"Delete",
				"Insert",
				"Escape",
				"ArrowUp",
				"ArrowDown",
				"ArrowLeft",
				"ArrowRight",
				"Home",
				"End",
				"PageUp",
				"PageDown",
				"CapsLock",
				"ShiftLeft",
				"ShiftRight",
				"ControlLeft",
				"ControlRight",
				"AltLeft",
				"AltRight",
				"MetaLeft",
				"MetaRight",
				"Minus",
				"Equal",
				"BracketLeft",
				"BracketRight",
				"Backslash",
				"Semicolon",
				"Quote",
				"Comma",
				"Period",
				"Slash",
				"Backquote",
			]
			.iter()
			.map(|s| s.to_string()),
		);

		for code in &codes {
			assert!(map_key(code).is_some(), "X11 map_key is missing canonical code {code}");
			assert!(
				evdev_keycode(code).is_some(),
				"Wayland evdev_keycode is missing canonical code {code}"
			);
		}
	}

	#[test]
	fn percent_decode_handles_escapes_and_passthrough() {
		assert_eq!(percent_decode("/tmp/a%20b.png"), "/tmp/a b.png");
		assert_eq!(percent_decode("/no/escapes"), "/no/escapes");
		assert_eq!(percent_decode("/weird%2"), "/weird%2"); // trailing/partial % left as-is
		assert_eq!(percent_decode("%2F%2f"), "//");
	}
}
