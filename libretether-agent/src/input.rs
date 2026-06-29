//! Input injection for live sessions, backed by `enigo`.
//!
//! `Enigo` holds platform handles that are not `Send`, so it lives on its own
//! dedicated OS thread. The session forwards events to it over a channel along
//! with the current capture geometry, which we use to turn the controller's
//! normalized (0.0–1.0) pointer coordinates into absolute screen pixels.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender};

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use libretether_protocol::{InputEvent, MouseButton};

/// A command sent to the injector thread.
pub enum InjectCmd {
	Geometry {
		width: u32,
		height: u32,
		/// Captured monitor's origin in the global desktop space — added to the
		/// scaled coordinate so input lands on the right monitor (see `inject`).
		origin_x: i32,
		origin_y: i32,
	},
	Event(InputEvent),
	Stop,
}

/// Spawn the injector thread and return a sender for [`InjectCmd`]s plus its
/// [`JoinHandle`](std::thread::JoinHandle). The thread exits on [`InjectCmd::Stop`]
/// or when every sender is dropped; the caller joins the handle at teardown so the
/// thread (and its `enigo` handles) don't outlive the session.
pub fn spawn() -> (Sender<InjectCmd>, std::thread::JoinHandle<()>) {
	let (tx, rx) = std::sync::mpsc::channel::<InjectCmd>();
	let handle = std::thread::spawn(move || run(rx));
	(tx, handle)
}

fn run(rx: Receiver<InjectCmd>) {
	let mut enigo = match Enigo::new(&Settings::default()) {
		Ok(e) => e,
		Err(e) => {
			crate::net::log(&format!("input injection unavailable: {e}"));
			// Drain so senders don't block, but do nothing.
			while let Ok(cmd) = rx.recv() {
				if matches!(cmd, InjectCmd::Stop) {
					break;
				}
			}
			return;
		}
	};

	let (mut w, mut h) = (1u32, 1u32);
	let (mut origin_x, mut origin_y) = (0i32, 0i32);
	let mut pressed = Pressed::default();
	while let Ok(cmd) = rx.recv() {
		match cmd {
			InjectCmd::Geometry {
				width,
				height,
				origin_x: ox,
				origin_y: oy,
			} => {
				w = width.max(1);
				h = height.max(1);
				origin_x = ox;
				origin_y = oy;
			}
			InjectCmd::Stop => break,
			InjectCmd::Event(ev) => {
				pressed.track(&ev);
				if let Err(e) = inject(&mut enigo, ev, origin_x, origin_y, w, h) {
					crate::net::log(&format!("inject error: {e}"));
				}
			}
		}
	}
	// The session is ending (explicit Stop, or the sender was dropped when the
	// connection died). Release anything still held — a modifier or mouse button
	// the controller had down at teardown would otherwise stay stuck *pressed* on
	// the remote machine until someone physically pressed and released it.
	release_all(&mut enigo, &pressed);
}

/// The keys and mouse buttons the controller currently holds down, so they can be
/// released if the session ends mid-press. Shared with the Wayland backend, which
/// tracks the same way but injects releases through the portal.
#[derive(Default)]
pub(crate) struct Pressed {
	pub(crate) keys: HashSet<String>,
	pub(crate) buttons: HashSet<MouseButton>,
}

impl Pressed {
	/// Fold an input event into the held set: a press records the key/button, a
	/// release clears it. Non-stateful events (moves, scroll, text) are ignored.
	pub(crate) fn track(&mut self, ev: &InputEvent) {
		match ev {
			InputEvent::Key { code, pressed: true } => {
				self.keys.insert(code.clone());
			}
			InputEvent::Key { code, pressed: false } => {
				self.keys.remove(code);
			}
			InputEvent::MouseButton { button, pressed: true } => {
				self.buttons.insert(*button);
			}
			InputEvent::MouseButton { button, pressed: false } => {
				self.buttons.remove(button);
			}
			_ => {}
		}
	}
}

/// Inject a release for every key/button still held in `pressed` (best-effort).
fn release_all(enigo: &mut Enigo, pressed: &Pressed) {
	for code in &pressed.keys {
		if let Some(key) = map_key(code) {
			let _ = enigo.key(key, Direction::Release);
		}
	}
	for &button in &pressed.buttons {
		let _ = enigo.button(map_button(button), Direction::Release);
	}
}

fn inject(
	enigo: &mut Enigo,
	ev: InputEvent,
	origin_x: i32,
	origin_y: i32,
	w: u32,
	h: u32,
) -> Result<(), enigo::InputError> {
	match ev {
		InputEvent::MouseMove { x, y } => {
			enigo.move_mouse(axis_px(origin_x, x, w), axis_px(origin_y, y, h), Coordinate::Abs)
		}
		InputEvent::MouseButton { button, pressed } => {
			let dir = if pressed { Direction::Press } else { Direction::Release };
			enigo.button(map_button(button), dir)
		}
		InputEvent::MouseScroll { dx, dy } => {
			if dx != 0 {
				enigo.scroll(dx, Axis::Horizontal)?;
			}
			if dy != 0 {
				enigo.scroll(dy, Axis::Vertical)?;
			}
			Ok(())
		}
		InputEvent::Key { code, pressed } => {
			let dir = if pressed { Direction::Press } else { Direction::Release };
			match map_key(&code) {
				Some(key) => enigo.key(key, dir),
				None => Ok(()),
			}
		}
		InputEvent::Text { text } => enigo.text(&text),
	}
}

/// Map a normalized (0.0–1.0) coordinate to an absolute pixel on an axis of the
/// given extent. The clamp keeps an out-of-range — or NaN — coordinate from the
/// (untrusted) controller from producing a wild or overflowing pixel: a NaN
/// clamps through to `0`, and values outside `[0,1]` saturate to the edges.
fn norm_to_px(v: f64, extent: u32) -> i32 {
	(v.clamp(0.0, 1.0) * extent as f64).round() as i32
}

/// Absolute virtual-desktop pixel for a normalized coordinate: the captured
/// monitor's `origin` plus the in-monitor offset. `enigo`'s `Coordinate::Abs`
/// addresses the whole virtual desktop, so without the origin a click on a monitor
/// that isn't at the virtual origin (a secondary display, or a primary placed
/// right-of/below another) would land on the wrong screen. `saturating_add` keeps
/// the result in range for any origin the compositor reports.
fn axis_px(origin: i32, v: f64, extent: u32) -> i32 {
	origin.saturating_add(norm_to_px(v, extent))
}

fn map_button(b: MouseButton) -> Button {
	match b {
		MouseButton::Left => Button::Left,
		MouseButton::Right => Button::Right,
		MouseButton::Middle => Button::Middle,
	}
}

/// Map a W3C `KeyboardEvent.code` to an `enigo::Key`. Returns `None` for codes we
/// don't recognise so the caller skips them — injecting a fallback character
/// (previously a space) for any unmapped key was worse than doing nothing.
///
/// Kept in lock-step with the Wayland backend's `evdev_keycode` (a parity test
/// asserts every canonical code maps in both), so a key works the same on X11 and
/// Wayland.
pub(crate) fn map_key(code: &str) -> Option<Key> {
	// Letters: "KeyA".."KeyZ".
	if let Some(letter) = code.strip_prefix("Key") {
		if letter.len() == 1 {
			if let Some(c) = letter.chars().next() {
				return Some(Key::Unicode(c.to_ascii_lowercase()));
			}
		}
	}
	// Digits: "Digit0".."Digit9".
	if let Some(d) = code.strip_prefix("Digit") {
		if let Some(c) = d.chars().next() {
			return Some(Key::Unicode(c));
		}
	}
	// Function keys: "F1".."F12".
	if let Some(n) = code.strip_prefix('F') {
		if let Ok(num) = n.parse::<u8>() {
			return Some(func_key(num));
		}
	}

	let key = match code {
		"Enter" | "NumpadEnter" => Key::Return,
		"Tab" => Key::Tab,
		"Space" => Key::Space,
		"Backspace" => Key::Backspace,
		"Delete" => Key::Delete,
		// enigo has no `Key::Insert` on macOS (Apple keyboards lack the key); fall
		// through to `None` there. The X11/Wayland parity test is Linux-only, so it
		// still enforces this arm on the platforms that have the variant.
		#[cfg(not(target_os = "macos"))]
		"Insert" => Key::Insert,
		"Escape" => Key::Escape,
		"ArrowUp" => Key::UpArrow,
		"ArrowDown" => Key::DownArrow,
		"ArrowLeft" => Key::LeftArrow,
		"ArrowRight" => Key::RightArrow,
		"Home" => Key::Home,
		"End" => Key::End,
		"PageUp" => Key::PageUp,
		"PageDown" => Key::PageDown,
		"CapsLock" => Key::CapsLock,
		"ShiftLeft" | "ShiftRight" => Key::Shift,
		"ControlLeft" | "ControlRight" => Key::Control,
		"AltLeft" | "AltRight" => Key::Alt,
		"MetaLeft" | "MetaRight" => Key::Meta,
		"Minus" => Key::Unicode('-'),
		"Equal" => Key::Unicode('='),
		"BracketLeft" => Key::Unicode('['),
		"BracketRight" => Key::Unicode(']'),
		"Backslash" => Key::Unicode('\\'),
		"Semicolon" => Key::Unicode(';'),
		"Quote" => Key::Unicode('\''),
		"Comma" => Key::Unicode(','),
		"Period" => Key::Unicode('.'),
		"Slash" => Key::Unicode('/'),
		"Backquote" => Key::Unicode('`'),
		_ => return None,
	};
	Some(key)
}

fn func_key(n: u8) -> Key {
	match n {
		1 => Key::F1,
		2 => Key::F2,
		3 => Key::F3,
		4 => Key::F4,
		5 => Key::F5,
		6 => Key::F6,
		7 => Key::F7,
		8 => Key::F8,
		9 => Key::F9,
		10 => Key::F10,
		11 => Key::F11,
		_ => Key::F12,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn maps_letters_digits_and_named_keys() {
		assert!(matches!(map_key("KeyA"), Some(Key::Unicode('a'))));
		assert!(matches!(map_key("KeyZ"), Some(Key::Unicode('z'))));
		assert!(matches!(map_key("Digit0"), Some(Key::Unicode('0'))));
		assert!(matches!(map_key("Enter"), Some(Key::Return)));
		assert!(matches!(map_key("NumpadEnter"), Some(Key::Return)));
		assert!(matches!(map_key("Space"), Some(Key::Space)));
		assert!(matches!(map_key("ShiftLeft"), Some(Key::Shift)));
		assert!(matches!(map_key("ArrowUp"), Some(Key::UpArrow)));
		assert!(matches!(map_key("Minus"), Some(Key::Unicode('-'))));
		assert!(matches!(map_key("Slash"), Some(Key::Unicode('/'))));
	}

	#[test]
	fn unknown_or_malformed_key_codes_map_to_none() {
		assert!(map_key("").is_none());
		assert!(map_key("Bogus").is_none());
		assert!(map_key("Key").is_none()); // "Key" with no letter
		assert!(map_key("KeyAB").is_none()); // not a single letter
	}

	#[test]
	fn function_keys_parse_and_clamp() {
		assert!(matches!(map_key("F1"), Some(Key::F1)));
		assert!(matches!(map_key("F12"), Some(Key::F12)));
		assert!(matches!(func_key(1), Key::F1));
		assert!(matches!(func_key(11), Key::F11));
		assert!(matches!(func_key(12), Key::F12));
		// Out-of-range numbers clamp to F12 rather than panicking.
		assert!(matches!(func_key(99), Key::F12));
	}

	#[test]
	fn maps_mouse_buttons() {
		assert!(matches!(map_button(MouseButton::Left), Button::Left));
		assert!(matches!(map_button(MouseButton::Right), Button::Right));
		assert!(matches!(map_button(MouseButton::Middle), Button::Middle));
	}

	// The held-state tracker is what lets the injector release a key/button that
	// was still down when the session ended (otherwise it stays stuck on the
	// remote machine). These pin its accounting.

	// Coordinate mapping runs on every pointer move with controller-supplied (and
	// therefore untrusted) normalized values — these pin the clamp + rounding and
	// prove a hostile NaN/out-of-range can't overflow the `as i32` cast.
	#[test]
	fn norm_to_px_maps_endpoints_and_midpoint() {
		assert_eq!(norm_to_px(0.0, 1920), 0);
		assert_eq!(norm_to_px(1.0, 1920), 1920);
		assert_eq!(norm_to_px(0.5, 1920), 960);
		assert_eq!(norm_to_px(0.5, 1081), 541); // rounds, not truncates
	}

	#[test]
	fn axis_px_offsets_by_the_monitor_origin() {
		// A monitor at the virtual origin behaves exactly like norm_to_px.
		assert_eq!(axis_px(0, 0.0, 1920), 0);
		assert_eq!(axis_px(0, 0.5, 1920), 960);
		// A secondary monitor at x-origin 1920: the same normalized x maps into that
		// monitor's slice of the virtual desktop, not the primary's.
		assert_eq!(axis_px(1920, 0.0, 1920), 1920);
		assert_eq!(axis_px(1920, 0.5, 1920), 2880);
		// A primary placed below another (y-origin 1080).
		assert_eq!(axis_px(1080, 1.0, 1080), 2160);
		// A NaN still can't overflow — norm_to_px clamps it to 0, then add the origin.
		assert_eq!(axis_px(1920, f64::NAN, 1920), 1920);
	}

	#[test]
	fn norm_to_px_clamps_out_of_range_and_nan() {
		assert_eq!(norm_to_px(-0.5, 1920), 0);
		assert_eq!(norm_to_px(1.5, 1920), 1920);
		assert_eq!(norm_to_px(f64::INFINITY, 1920), 1920);
		assert_eq!(norm_to_px(f64::NEG_INFINITY, 1920), 0);
		// NaN must not panic or produce garbage — the saturating cast yields 0.
		assert_eq!(norm_to_px(f64::NAN, 1920), 0);
	}

	#[test]
	fn pressed_tracks_keys_down_and_up() {
		let mut p = Pressed::default();
		p.track(&InputEvent::Key {
			code: "ShiftLeft".into(),
			pressed: true,
		});
		p.track(&InputEvent::Key {
			code: "KeyA".into(),
			pressed: true,
		});
		assert!(p.keys.contains("ShiftLeft") && p.keys.contains("KeyA"));
		// A release clears just that key.
		p.track(&InputEvent::Key {
			code: "KeyA".into(),
			pressed: false,
		});
		assert!(p.keys.contains("ShiftLeft") && !p.keys.contains("KeyA"));
	}

	#[test]
	fn pressed_tracks_mouse_buttons_down_and_up() {
		let mut p = Pressed::default();
		p.track(&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: true,
		});
		p.track(&InputEvent::MouseButton {
			button: MouseButton::Right,
			pressed: true,
		});
		assert_eq!(p.buttons.len(), 2);
		p.track(&InputEvent::MouseButton {
			button: MouseButton::Left,
			pressed: false,
		});
		assert!(p.buttons.contains(&MouseButton::Right) && !p.buttons.contains(&MouseButton::Left));
	}

	#[test]
	fn pressed_ignores_non_stateful_events() {
		let mut p = Pressed::default();
		p.track(&InputEvent::MouseMove { x: 0.5, y: 0.5 });
		p.track(&InputEvent::MouseScroll { dx: 1, dy: -1 });
		p.track(&InputEvent::Text { text: "hi".into() });
		assert!(p.keys.is_empty() && p.buttons.is_empty());
	}

	#[test]
	fn pressed_leaves_only_still_held_keys_at_teardown() {
		// A full press/release pair leaves nothing held; a lone press remains, so
		// release_all only releases what's actually still down.
		let mut p = Pressed::default();
		p.track(&InputEvent::Key {
			code: "KeyA".into(),
			pressed: true,
		});
		p.track(&InputEvent::Key {
			code: "KeyA".into(),
			pressed: false,
		});
		p.track(&InputEvent::Key {
			code: "ControlLeft".into(),
			pressed: true,
		});
		assert_eq!(p.keys.iter().collect::<Vec<_>>(), vec!["ControlLeft"]);
	}
}
