//! Input injection for live sessions, backed by `enigo`.
//!
//! `Enigo` holds platform handles that are not `Send`, so it lives on its own
//! dedicated OS thread. The session forwards events to it over a channel along
//! with the current capture geometry, which we use to turn the controller's
//! normalized (0.0–1.0) pointer coordinates into absolute screen pixels.

use std::sync::mpsc::{Receiver, Sender};

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use tether_protocol::{InputEvent, MouseButton};

/// A command sent to the injector thread.
pub enum InjectCmd {
	Geometry { width: u32, height: u32 },
	Event(InputEvent),
	Stop,
}

/// Spawn the injector thread and return a sender for [`InjectCmd`]s. The thread
/// exits on [`InjectCmd::Stop`] or when the sender is dropped.
pub fn spawn() -> Sender<InjectCmd> {
	let (tx, rx) = std::sync::mpsc::channel::<InjectCmd>();
	std::thread::spawn(move || run(rx));
	tx
}

fn run(rx: Receiver<InjectCmd>) {
	let mut enigo = match Enigo::new(&Settings::default()) {
		Ok(e) => e,
		Err(e) => {
			eprintln!("[tether-agent] input injection unavailable: {e}");
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
	while let Ok(cmd) = rx.recv() {
		match cmd {
			InjectCmd::Geometry { width, height } => {
				w = width.max(1);
				h = height.max(1);
			}
			InjectCmd::Stop => break,
			InjectCmd::Event(ev) => {
				if let Err(e) = inject(&mut enigo, ev, w, h) {
					eprintln!("[tether-agent] inject error: {e}");
				}
			}
		}
	}
}

fn inject(enigo: &mut Enigo, ev: InputEvent, w: u32, h: u32) -> Result<(), enigo::InputError> {
	match ev {
		InputEvent::MouseMove { x, y } => {
			let px = (x.clamp(0.0, 1.0) * w as f64).round() as i32;
			let py = (y.clamp(0.0, 1.0) * h as f64).round() as i32;
			enigo.move_mouse(px, py, Coordinate::Abs)
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
			enigo.key(map_key(&code), dir)
		}
		InputEvent::Text { text } => enigo.text(&text),
	}
}

fn map_button(b: MouseButton) -> Button {
	match b {
		MouseButton::Left => Button::Left,
		MouseButton::Right => Button::Right,
		MouseButton::Middle => Button::Middle,
	}
}

/// Map a W3C `KeyboardEvent.code` to an `enigo::Key`.
fn map_key(code: &str) -> Key {
	// Letters: "KeyA".."KeyZ".
	if let Some(letter) = code.strip_prefix("Key") {
		if letter.len() == 1 {
			if let Some(c) = letter.chars().next() {
				return Key::Unicode(c.to_ascii_lowercase());
			}
		}
	}
	// Digits: "Digit0".."Digit9".
	if let Some(d) = code.strip_prefix("Digit") {
		if let Some(c) = d.chars().next() {
			return Key::Unicode(c);
		}
	}
	// Function keys: "F1".."F12".
	if let Some(n) = code.strip_prefix('F') {
		if let Ok(num) = n.parse::<u8>() {
			return func_key(num);
		}
	}

	match code {
		"Enter" | "NumpadEnter" => Key::Return,
		"Tab" => Key::Tab,
		"Space" => Key::Space,
		"Backspace" => Key::Backspace,
		"Delete" => Key::Delete,
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
		_ => Key::Unicode(' '),
	}
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
