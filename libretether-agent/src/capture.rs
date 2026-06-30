//! Screen capture, backed by `xcap`. Capture is blocking and the monitor
//! handles are not `Send`, so every call here is meant to run on a dedicated
//! blocking thread (`spawn_blocking` or a plain `std::thread`), never across an
//! `.await`.

use std::io::Cursor;

use anyhow::{anyhow, Result};
use image::RgbaImage;
use xcap::Monitor;

/// A captured frame plus the geometry it came from.
pub struct Capture {
	pub width: u32,
	pub height: u32,
	/// The monitor's top-left origin in the global/virtual-desktop coordinate
	/// space. Non-zero for any monitor that isn't at the virtual origin (a
	/// secondary display, or a primary placed right of/below another). Input
	/// injection uses `enigo`'s absolute (virtual-desktop) coordinates, so it must
	/// add this origin — otherwise pointer events on such a monitor land on the
	/// wrong screen.
	pub origin_x: i32,
	pub origin_y: i32,
	pub image: RgbaImage,
}

/// Number of monitors currently attached, or 0 if enumeration fails.
pub fn display_count() -> u32 {
	// On Wayland `xcap` enumerates over X11, which fails noisily ("Authorization
	// required…") without an X session. We capture a single portal stream there,
	// so report one display and skip the X11 probe entirely.
	#[cfg(target_os = "linux")]
	if crate::platform::is_wayland() {
		return 1;
	}
	Monitor::all().map(|m| m.len() as u32).unwrap_or(0)
}

/// Capture a single display by index. Index 0 is the primary monitor when one
/// is flagged as primary, otherwise the first enumerated monitor.
pub fn capture(display: u32) -> Result<Capture> {
	let monitors = Monitor::all().map_err(|e| anyhow!("enumerating monitors: {e}"))?;
	if monitors.is_empty() {
		return Err(anyhow!("no monitors found (is a display attached / session active?)"));
	}

	let monitor = pick(&monitors, display)?;
	let (origin_x, origin_y) = (monitor.x(), monitor.y());
	let image = monitor
		.capture_image()
		.map_err(|e| anyhow!("capturing display {display}: {e}"))?;
	let (width, height) = (image.width(), image.height());
	Ok(Capture {
		width,
		height,
		origin_x,
		origin_y,
		image,
	})
}

fn pick(monitors: &[Monitor], display: u32) -> Result<&Monitor> {
	if display == 0 {
		// Prefer the primary monitor for the default display.
		if let Some(primary) = monitors.iter().find(|m| m.is_primary()) {
			return Ok(primary);
		}
		return Ok(&monitors[0]);
	}
	monitors
		.get(display as usize)
		.ok_or_else(|| anyhow!("display {display} out of range (have {})", monitors.len()))
}

/// Encode an RGBA image as PNG.
pub fn encode_png(image: &RgbaImage) -> Result<Vec<u8>> {
	let mut buf = Cursor::new(Vec::new());
	image.write_to(&mut buf, image::ImageFormat::Png)?;
	Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn encode_png_preserves_dimensions() {
		let img = RgbaImage::from_pixel(2, 5, image::Rgba([1, 2, 3, 4]));
		let decoded = image::load_from_memory(&encode_png(&img).unwrap()).unwrap();
		assert_eq!((decoded.width(), decoded.height()), (2, 5));
	}
}
