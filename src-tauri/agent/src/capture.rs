//! Screen capture, backed by `xcap`. Capture is blocking and the monitor
//! handles are not `Send`, so every call here is meant to run on a dedicated
//! blocking thread (`spawn_blocking` or a plain `std::thread`), never across an
//! `.await`.

use std::io::Cursor;

use anyhow::{anyhow, Result};
use image::codecs::jpeg::JpegEncoder;
use image::{ImageEncoder, RgbaImage};
use xcap::Monitor;

/// A captured frame plus the geometry it came from.
pub struct Capture {
	pub width: u32,
	pub height: u32,
	pub image: RgbaImage,
}

/// Number of monitors currently attached, or 0 if enumeration fails.
pub fn display_count() -> u32 {
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
	let image = monitor
		.capture_image()
		.map_err(|e| anyhow!("capturing display {display}: {e}"))?;
	let (width, height) = (image.width(), image.height());
	Ok(Capture { width, height, image })
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

/// Encode an RGBA image as JPEG (RGB, no alpha).
pub fn encode_jpeg(image: &RgbaImage, quality: u8) -> Result<Vec<u8>> {
	let rgb = image::DynamicImage::ImageRgba8(image.clone()).to_rgb8();
	let mut buf = Vec::new();
	JpegEncoder::new_with_quality(&mut buf, quality.clamp(1, 100)).write_image(
		rgb.as_raw(),
		rgb.width(),
		rgb.height(),
		image::ExtendedColorType::Rgb8,
	)?;
	Ok(buf)
}

/// Encode an RGBA image as PNG.
pub fn encode_png(image: &RgbaImage) -> Result<Vec<u8>> {
	let mut buf = Cursor::new(Vec::new());
	image.write_to(&mut buf, image::ImageFormat::Png)?;
	Ok(buf.into_inner())
}
