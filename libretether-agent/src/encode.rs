//! The capture→encode→write pipeline's middle stage, shared by the X11 and
//! Wayland backends.
//!
//! A capture source hands raw RGBA frames in on a single-slot channel (newest
//! wins; stale frames are dropped to stay real-time). This stage downscales,
//! splits the frame into a grid of tiles, hashes each tile to find what changed
//! since the last frame, JPEG-encodes only the changed tiles (in parallel across
//! cores), and writes a binary [`libretether_protocol::video`] frame out. Full
//! keyframes are sent on start, on a geometry/scale change, and on an explicit
//! refresh.
//!
//! Running this off the capture thread is what unblocks the frame rate: capturing
//! the next frame overlaps encoding the current one, instead of the old serial
//! "capture → encode → send → sleep" loop that capped throughput regardless of
//! how fast the machine was.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use image::imageops::{self, FilterType};
use image::RgbaImage;
use jpeg_encoder::{ColorType, Encoder};
use libretether_protocol::video::{self, Tile};
use libretether_protocol::SessionConfig;
use rayon::prelude::*;

/// Grid cell size, in source pixels. 256 keeps per-tile JPEG header overhead
/// small while still giving delta encoding useful granularity (a moving cursor or
/// a typing caret only dirties one or two tiles).
const TILE_SIZE: u16 = 256;

/// Adaptive-mode bounds: never drop the effective scale below this, and step it
/// by this much. A raise only happens after a streak of comfortable frames so the
/// scale doesn't flap (each change forces a keyframe).
const AUTO_MIN_SCALE: u8 = 40;
const AUTO_STEP: u8 = 10;
const AUTO_RAISE_AFTER: u32 = 30;

/// Live, lock-free view of the session knobs, shared between the capture thread
/// (reads `max_fps`), the encoder thread (reads quality/scale/auto), and the
/// session reader (writes them on `Configure`/`Refresh`).
pub struct SharedConfig {
	quality: AtomicU8,
	scale: AtomicU8,
	max_fps: AtomicU8,
	auto: AtomicBool,
	force_key: AtomicBool,
}

impl SharedConfig {
	pub fn new(cfg: &SessionConfig) -> Arc<Self> {
		Arc::new(Self {
			quality: AtomicU8::new(cfg.quality),
			scale: AtomicU8::new(cfg.scale),
			max_fps: AtomicU8::new(cfg.max_fps),
			auto: AtomicBool::new(cfg.auto),
			// The first frame of a session is always a full keyframe.
			force_key: AtomicBool::new(true),
		})
	}

	/// Apply a new (already-sanitized) config live. Always forces a keyframe so the
	/// new quality/scale take effect cleanly (and a scale change resizes the canvas).
	pub fn apply(&self, cfg: &SessionConfig) {
		self.quality.store(cfg.quality, Ordering::Relaxed);
		self.scale.store(cfg.scale, Ordering::Relaxed);
		self.max_fps.store(cfg.max_fps, Ordering::Relaxed);
		self.auto.store(cfg.auto, Ordering::Relaxed);
		self.request_keyframe();
	}

	pub fn request_keyframe(&self) {
		self.force_key.store(true, Ordering::Relaxed);
	}

	fn take_force_key(&self) -> bool {
		self.force_key.swap(false, Ordering::Relaxed)
	}

	fn quality(&self) -> u8 {
		self.quality.load(Ordering::Relaxed)
	}

	fn scale(&self) -> u8 {
		self.scale.load(Ordering::Relaxed)
	}

	fn auto(&self) -> bool {
		self.auto.load(Ordering::Relaxed)
	}

	/// Frame interval target, clamped to a sane range (the capture threads use this).
	pub fn max_fps(&self) -> u64 {
		self.max_fps.load(Ordering::Relaxed).clamp(1, 60) as u64
	}
}

/// A captured frame from either backend, before downscale/encode.
pub struct RawFrame {
	pub width: u32,
	pub height: u32,
	/// Captured monitor's origin in the global desktop space (X11 multi-monitor);
	/// 0,0 for the Wayland portal stream.
	pub origin_x: i32,
	pub origin_y: i32,
	pub rgba: RgbaImage,
	/// Microseconds the producing thread spent obtaining this frame (the xcap grab
	/// on X11/Windows/macOS, or the PipeWire→RGBA conversion on Wayland) — fed into
	/// the per-stage stats so the capture cost is visible alongside encode/network.
	pub capture_us: u64,
}

/// Per-stage timing + tile counts for one frame, returned by [`Tiler::encode`].
pub struct EncodeOutcome {
	/// The frame body, or `None` if nothing changed (the hash still ran).
	pub body: Option<Vec<u8>>,
	pub hash_us: u64,
	pub encode_us: u64,
	pub total_tiles: u32,
	pub changed_tiles: u32,
}

/// An encoded frame ready to write to the session stream.
pub struct OutFrame {
	/// Source (pre-downscale) geometry, used for the `Meta` message and input mapping.
	pub source_width: u32,
	pub source_height: u32,
	pub origin_x: i32,
	pub origin_y: i32,
	/// The full binary frame message body (see [`libretether_protocol::video`]).
	pub body: Vec<u8>,
}

/// Run the encoder stage to completion. Returns when the capture side hangs up
/// (`rx` closed) or the writer side goes away (`tx` closed).
pub fn run(
	rx: std::sync::mpsc::Receiver<RawFrame>,
	tx: tokio::sync::mpsc::Sender<OutFrame>,
	shared: Arc<SharedConfig>,
) {
	let mut tiler = Tiler::new(TILE_SIZE);
	let mut seq = 0u64;
	// Effective scale ceiling for adaptive mode; tracks the configured scale when
	// auto is off.
	let mut eff_scale = shared.scale();
	let mut good_streak = 0u32;
	let mut stats = Stats::new();

	while let Ok(raw) = rx.recv() {
		let quality = shared.quality();
		let ceiling = shared.scale();
		let auto = shared.auto();
		eff_scale = if auto {
			eff_scale.clamp(AUTO_MIN_SCALE, ceiling)
		} else {
			ceiling
		};

		if shared.take_force_key() {
			tiler.reset();
		}

		let down_started = Instant::now();
		let scaled = downscale(&raw.rgba, eff_scale);
		let downscale_us = down_started.elapsed().as_micros() as u64;

		let mut outcome = tiler.encode(&scaled, quality, &mut seq);

		// Send only when something changed (a static frame produces no body). Take the
		// body out so the rest of `outcome` (its timings/counts) stays borrowable for
		// the stats below.
		let body = outcome.body.take();
		let bytes = body.as_ref().map_or(0, |b| b.len() as u64);
		let sent = body.is_some();
		let mut send_us = 0u64;
		if let Some(body) = body {
			let out = OutFrame {
				source_width: raw.width,
				source_height: raw.height,
				origin_x: raw.origin_x,
				origin_y: raw.origin_y,
				body,
			};
			// Backpressure (not frame-dropping): a delta frame must reach the
			// controller or its canvas goes stale, so block here. How long we block
			// measures how far behind the link is.
			let send_started = Instant::now();
			if tx.blocking_send(out).is_err() {
				break;
			}
			send_us = send_started.elapsed().as_micros() as u64;
		}

		// Adaptive scale reacts to encode/network only — capture and downscale don't
		// shrink with scale (downscale runs *after* capture), so lowering scale can't
		// fix a capture-bound stream.
		if auto {
			let interval_us = 1_000_000 / shared.max_fps();
			let behind = outcome.encode_us > interval_us || send_us > interval_us;
			if behind && eff_scale > AUTO_MIN_SCALE {
				eff_scale = eff_scale.saturating_sub(AUTO_STEP).max(AUTO_MIN_SCALE);
				good_streak = 0;
			} else if !behind {
				good_streak += 1;
				if good_streak >= AUTO_RAISE_AFTER && eff_scale < ceiling {
					eff_scale = (eff_scale + AUTO_STEP).min(ceiling);
					good_streak = 0;
				}
			}
		}

		stats.record(&raw, downscale_us, &outcome, send_us, bytes, sent);
		stats.maybe_log(eff_scale, quality);
	}
}

/// Rolling per-stage timing, logged ~once a second while a session runs so the
/// operator can see exactly where the frame budget goes (capture vs hash vs encode
/// vs network) on each guest — the basis for deciding what to optimize next.
struct Stats {
	window_start: Instant,
	frames: u32,
	sent: u32,
	capture_us: u64,
	downscale_us: u64,
	hash_us: u64,
	encode_us: u64,
	send_us: u64,
	changed_tiles: u64,
	total_tiles: u64,
	bytes: u64,
}

impl Stats {
	fn new() -> Self {
		Self {
			window_start: Instant::now(),
			frames: 0,
			sent: 0,
			capture_us: 0,
			downscale_us: 0,
			hash_us: 0,
			encode_us: 0,
			send_us: 0,
			changed_tiles: 0,
			total_tiles: 0,
			bytes: 0,
		}
	}

	fn record(
		&mut self,
		raw: &RawFrame,
		downscale_us: u64,
		outcome: &EncodeOutcome,
		send_us: u64,
		bytes: u64,
		sent: bool,
	) {
		self.frames += 1;
		if sent {
			self.sent += 1;
		}
		self.capture_us += raw.capture_us;
		self.downscale_us += downscale_us;
		self.hash_us += outcome.hash_us;
		self.encode_us += outcome.encode_us;
		self.send_us += send_us;
		self.changed_tiles += outcome.changed_tiles as u64;
		self.total_tiles += outcome.total_tiles as u64;
		self.bytes += bytes;
	}

	fn maybe_log(&mut self, scale: u8, quality: u8) {
		let elapsed = self.window_start.elapsed();
		if elapsed < Duration::from_secs(1) || self.frames == 0 {
			return;
		}
		let n = self.frames as f64;
		// Mean milliseconds per processed frame for each stage.
		let ms = |total: u64| (total as f64 / n) / 1000.0;
		let fps = n / elapsed.as_secs_f64();
		let kib_per_sent = (self.bytes as f64 / self.sent.max(1) as f64) / 1024.0;
		crate::net::log(&format!(
			"stream {fps:.0} fps ({} sent/s) | cap {:.1} down {:.1} hash {:.1} enc {:.1} net {:.1} ms/f | {:.0}/{:.0} tiles {kib_per_sent:.0} KiB/f | scale {scale}% q{quality}",
			self.sent,
			ms(self.capture_us),
			ms(self.downscale_us),
			ms(self.hash_us),
			ms(self.encode_us),
			ms(self.send_us),
			self.changed_tiles as f64 / n,
			self.total_tiles as f64 / n,
		));
		*self = Stats::new();
	}
}

/// Downscale by `scale` percent, borrowing the original untouched at 100%.
fn downscale(img: &RgbaImage, scale: u8) -> Cow<'_, RgbaImage> {
	if scale >= 100 {
		return Cow::Borrowed(img);
	}
	let nw = (img.width() * scale as u32 / 100).max(1);
	let nh = (img.height() * scale as u32 / 100).max(1);
	Cow::Owned(imageops::resize(img, nw, nh, FilterType::Triangle))
}

/// Splits a frame into a tile grid, tracks per-tile content hashes, and emits
/// only changed tiles between keyframes.
pub struct Tiler {
	tile: u16,
	width: u32,
	height: u32,
	cols: u32,
	rows: u32,
	/// Per-tile content hash from the last emitted frame, row-major `cols × rows`.
	hashes: Vec<u64>,
}

impl Tiler {
	pub fn new(tile: u16) -> Self {
		Self {
			tile,
			width: 0,
			height: 0,
			cols: 0,
			rows: 0,
			hashes: Vec::new(),
		}
	}

	/// Force the next frame to be a full keyframe (geometry/scale/quality changed).
	pub fn reset(&mut self) {
		self.hashes.clear();
		self.width = 0;
		self.height = 0;
	}

	/// Encode `img` into a binary frame body (with per-stage timings), or a body of
	/// `None` if nothing changed since the previous frame. Bumps `seq` only when a
	/// frame is actually produced.
	pub fn encode(&mut self, img: &RgbaImage, quality: u8, seq: &mut u64) -> EncodeOutcome {
		let (w, h) = (img.width(), img.height());
		let tile = self.tile as u32;
		let key = w != self.width || h != self.height || self.hashes.is_empty();
		if key {
			self.width = w;
			self.height = h;
			self.cols = w.div_ceil(tile);
			self.rows = h.div_ceil(tile);
			self.hashes = vec![0u64; (self.cols * self.rows) as usize];
		}
		let (cols, rows) = (self.cols, self.rows);
		let count = (cols * rows) as usize;

		// Hash every tile (parallel, read-only over the frame).
		let hash_started = Instant::now();
		let current: Vec<u64> = (0..count)
			.into_par_iter()
			.map(|idx| {
				let rect = tile_rect(idx as u32, cols, tile, w, h);
				hash_tile(img, rect)
			})
			.collect();
		let hash_us = hash_started.elapsed().as_micros() as u64;

		// On a keyframe every tile is sent; otherwise only those whose hash moved.
		let changed: Vec<usize> = if key {
			(0..count).collect()
		} else {
			(0..count).filter(|&i| current[i] != self.hashes[i]).collect()
		};
		if changed.is_empty() {
			return EncodeOutcome {
				body: None,
				hash_us,
				encode_us: 0,
				total_tiles: count as u32,
				changed_tiles: 0,
			};
		}

		// Encode the selected tiles in parallel.
		let encode_started = Instant::now();
		let tiles: Vec<Tile> = changed
			.par_iter()
			.map(|&i| {
				let rect = tile_rect(i as u32, cols, tile, w, h);
				Tile {
					col: (i as u32 % cols) as u16,
					row: (i as u32 / cols) as u16,
					jpeg: encode_region(img, rect, quality),
				}
			})
			.collect();
		let encode_us = encode_started.elapsed().as_micros() as u64;

		self.hashes = current;
		*seq += 1;
		EncodeOutcome {
			body: Some(video::frame_message(key, *seq, w, h, self.tile, &tiles)),
			hash_us,
			encode_us,
			total_tiles: count as u32,
			changed_tiles: changed.len() as u32,
		}
	}
}

/// A tile's pixel rectangle, clamped to the frame's edges.
struct Rect {
	x: u32,
	y: u32,
	w: u32,
	h: u32,
}

fn tile_rect(idx: u32, cols: u32, tile: u32, fw: u32, fh: u32) -> Rect {
	let (cx, cy) = (idx % cols, idx / cols);
	let (x, y) = (cx * tile, cy * tile);
	Rect {
		x,
		y,
		w: tile.min(fw - x),
		h: tile.min(fh - y),
	}
}

/// FNV-1a over a tile's pixels — fast, and a 64-bit hash makes a missed update
/// (a collision) astronomically unlikely.
fn hash_tile(img: &RgbaImage, rect: Rect) -> u64 {
	let stride = img.width() as usize * 4;
	let raw = img.as_raw();
	let row_bytes = rect.w as usize * 4;
	let mut h: u64 = 0xcbf2_9ce4_8422_2325;
	for row in 0..rect.h {
		let start = (rect.y + row) as usize * stride + rect.x as usize * 4;
		let bytes = &raw[start..start + row_bytes];
		let mut chunks = bytes.chunks_exact(8);
		for c in &mut chunks {
			let v = u64::from_le_bytes(c.try_into().unwrap());
			h = (h ^ v).wrapping_mul(0x0000_0100_0000_01b3);
		}
		for &b in chunks.remainder() {
			h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
		}
	}
	h
}

/// Copy a tile out of the frame into a tight RGBA buffer and JPEG-encode it. The
/// encoder ignores the alpha channel, so no separate RGB conversion is needed.
fn encode_region(img: &RgbaImage, rect: Rect, quality: u8) -> Vec<u8> {
	let stride = img.width() as usize * 4;
	let raw = img.as_raw();
	let row_bytes = rect.w as usize * 4;
	let mut buf = Vec::with_capacity(row_bytes * rect.h as usize);
	for row in 0..rect.h {
		let start = (rect.y + row) as usize * stride + rect.x as usize * 4;
		buf.extend_from_slice(&raw[start..start + row_bytes]);
	}
	let mut out = Vec::new();
	let encoder = Encoder::new(&mut out, quality);
	// width/height fit u16 comfortably — a tile is at most TILE_SIZE px.
	encoder
		.encode(&buf, rect.w as u16, rect.h as u16, ColorType::Rgba)
		.expect("jpeg encode of an in-bounds tile cannot fail");
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use libretether_protocol::video::{Inbound, KIND_DELTA, KIND_KEY};

	fn solid(w: u32, h: u32, px: [u8; 4]) -> RgbaImage {
		RgbaImage::from_pixel(w, h, image::Rgba(px))
	}

	async fn read_back(body: &[u8]) -> Vec<u8> {
		let (mut a, mut b) = tokio::io::duplex(1 << 20);
		video::write_message(&mut a, body).await.unwrap();
		match video::read_inbound(&mut b).await.unwrap() {
			Inbound::Frame(payload) => payload,
			_ => panic!("expected a frame"),
		}
	}

	#[tokio::test]
	async fn first_frame_is_a_keyframe_with_every_tile() {
		let mut tiler = Tiler::new(256);
		let mut seq = 0;
		// 300×300 with tile 256 → a 2×2 grid (4 tiles).
		let body = tiler
			.encode(&solid(300, 300, [10, 20, 30, 255]), 70, &mut seq)
			.body
			.unwrap();
		let payload = read_back(&body).await;
		assert_eq!(payload[0], KIND_KEY);
		assert_eq!(seq, 1);
		let count = u32::from_be_bytes(payload[19..23].try_into().unwrap());
		assert_eq!(count, 4, "keyframe must carry all 4 tiles");
	}

	#[tokio::test]
	async fn unchanged_frame_emits_nothing() {
		let mut tiler = Tiler::new(256);
		let mut seq = 0;
		let img = solid(300, 300, [1, 2, 3, 255]);
		tiler.encode(&img, 70, &mut seq).body.unwrap();
		assert!(
			tiler.encode(&img, 70, &mut seq).body.is_none(),
			"a static frame sends nothing"
		);
		assert_eq!(seq, 1);
	}

	#[tokio::test]
	async fn delta_sends_only_changed_tiles() {
		let mut tiler = Tiler::new(256);
		let mut seq = 0;
		let mut img = solid(300, 300, [0, 0, 0, 255]);
		tiler.encode(&img, 70, &mut seq).body.unwrap(); // keyframe

		// Touch one pixel in the top-left tile only.
		img.put_pixel(5, 5, image::Rgba([255, 255, 255, 255]));
		let body = tiler.encode(&img, 70, &mut seq).body.unwrap();
		let payload = read_back(&body).await;
		assert_eq!(payload[0], KIND_DELTA);
		let count = u32::from_be_bytes(payload[19..23].try_into().unwrap());
		assert_eq!(count, 1, "only the dirtied tile should be sent");
		// And it is the (0,0) tile.
		assert_eq!(u16::from_be_bytes(payload[23..25].try_into().unwrap()), 0);
		assert_eq!(u16::from_be_bytes(payload[25..27].try_into().unwrap()), 0);
	}

	#[tokio::test]
	async fn reset_forces_a_fresh_keyframe() {
		let mut tiler = Tiler::new(256);
		let mut seq = 0;
		let img = solid(300, 300, [9, 9, 9, 255]);
		tiler.encode(&img, 70, &mut seq).body.unwrap();
		tiler.reset();
		let body = tiler.encode(&img, 70, &mut seq).body.unwrap();
		let payload = read_back(&body).await;
		assert_eq!(payload[0], KIND_KEY, "after reset the next frame is a keyframe");
	}

	#[test]
	fn downscale_halves_dimensions() {
		let img = solid(200, 100, [0, 0, 0, 255]);
		let small = downscale(&img, 50);
		assert_eq!((small.width(), small.height()), (100, 50));
		// 100% borrows the original (same dimensions, no resize).
		assert_eq!(downscale(&img, 100).dimensions(), (200, 100));
	}
}
