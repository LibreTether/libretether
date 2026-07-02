//! The capture→encode→write pipeline's middle stage, shared by the X11, Windows
//! and Wayland backends.
//!
//! A capture source hands raw RGBA frames in on a single-slot channel (newest
//! wins; stale frames are dropped to stay real-time). This stage downscales to an
//! even-dimensioned canvas, converts to I420, and feeds an **inter-frame H.264
//! encoder** (OpenH264). Only what *moved* between frames costs bits — the codec's
//! motion estimation replaces the old per-tile dirty-rectangle scheme — and a
//! cheap whole-frame hash still short-circuits a perfectly static screen to zero
//! bandwidth. Each encoded access unit is written as a binary
//! [`libretether_protocol::video`] frame; the controller decodes it with WebCodecs.
//!
//! Running this off the capture thread is what unblocks the frame rate: capturing
//! the next frame overlaps encoding the current one, instead of the old serial
//! "capture → encode → send → sleep" loop.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use image::imageops::{self, FilterType};
use image::RgbaImage;
use libretether_protocol::video;
use libretether_protocol::SessionConfig;
use openh264::encoder::{
	BitRate, Encoder, EncoderConfig, FrameRate, FrameType, Profile, RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;
use rayon::prelude::*;

/// Adaptive-mode bounds: never drop the effective scale below this, and step it
/// by this much. A raise only happens after a streak of comfortable frames so the
/// scale doesn't flap (each change re-inits the encoder and forces a keyframe).
const AUTO_MIN_SCALE: u8 = 40;
const AUTO_STEP: u8 = 10;
const AUTO_RAISE_AFTER: u32 = 30;

/// Live, lock-free view of the session knobs, shared between the capture thread
/// (reads `max_fps`), the encoder thread (reads bitrate/scale/auto), and the
/// session reader (writes them on `Configure`/`Refresh`).
pub struct SharedConfig {
	bitrate_kbps: AtomicU32,
	scale: AtomicU8,
	max_fps: AtomicU8,
	auto: AtomicBool,
	force_key: AtomicBool,
	/// The actual capture backend, reported by whichever capture thread wins (DXGI
	/// vs GDI on Windows, xcap elsewhere, PipeWire on Wayland), and the actual video
	/// encoder, reported by the encode thread. Set once, read by the session's Meta
	/// send so the controller can display exactly what's running.
	capture_backend: OnceLock<&'static str>,
	encoder_backend: OnceLock<&'static str>,
}

impl SharedConfig {
	pub fn new(cfg: &SessionConfig) -> Arc<Self> {
		Arc::new(Self {
			bitrate_kbps: AtomicU32::new(cfg.bitrate_kbps),
			scale: AtomicU8::new(cfg.scale),
			max_fps: AtomicU8::new(cfg.max_fps),
			auto: AtomicBool::new(cfg.auto),
			// The first frame of a session is always a full keyframe.
			force_key: AtomicBool::new(true),
			capture_backend: OnceLock::new(),
			encoder_backend: OnceLock::new(),
		})
	}

	/// Record the capture/encoder backend actually in use (first writer wins).
	pub fn report_capture(&self, name: &'static str) {
		let _ = self.capture_backend.set(name);
	}
	pub fn report_encoder(&self, name: &'static str) {
		let _ = self.encoder_backend.set(name);
	}
	/// The reported backends, or "unknown" before the first frame has flowed.
	pub fn capture_backend(&self) -> &'static str {
		self.capture_backend.get().copied().unwrap_or("unknown")
	}
	pub fn encoder_backend(&self) -> &'static str {
		self.encoder_backend.get().copied().unwrap_or("unknown")
	}

	/// Apply a new (already-sanitized) config live. Always forces a keyframe so the
	/// new settings take effect cleanly.
	pub fn apply(&self, cfg: &SessionConfig) {
		self.bitrate_kbps.store(cfg.bitrate_kbps, Ordering::Relaxed);
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

	fn bitrate_kbps(&self) -> u32 {
		self.bitrate_kbps.load(Ordering::Relaxed)
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

/// A captured frame from any backend, before downscale/encode.
pub struct RawFrame {
	pub width: u32,
	pub height: u32,
	/// Captured monitor's origin in the global desktop space (X11 multi-monitor);
	/// 0,0 for the Wayland portal stream.
	pub origin_x: i32,
	pub origin_y: i32,
	pub rgba: RgbaImage,
	/// Microseconds the producing thread spent obtaining this frame — fed into the
	/// per-stage stats so capture cost is visible alongside encode/network.
	pub capture_us: u64,
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
	let mut seq = 0u64;
	let mut eff_scale = shared.scale();
	let mut good_streak = 0u32;
	let mut stats = Stats::new();

	// The encoder is rebuilt when its rate-control inputs (bitrate/fps) or the coded
	// dimensions change; each backend is built for a fixed resolution and a rebuild
	// starts a fresh keyframe. On Windows a hardware Media Foundation encoder is used
	// when available (see `build_encoder`), otherwise software OpenH264.
	let mut enc: Option<Box<dyn ScreenEncoder>> = None;
	let mut built = (0u32, 0u8, 0u32, 0u32); // (bitrate, fps, width, height)
	let mut last_hash: Option<u64> = None;
	let mut last_dims = (0u32, 0u32);
	// The active backend, logged once (and again only if it ever changes) so the
	// Logs page shows exactly which encoder is running.
	let mut announced: Option<&'static str> = None;

	while let Ok(raw) = rx.recv() {
		let bitrate = shared.bitrate_kbps();
		let fps = shared.max_fps() as u8;
		let ceiling = shared.scale();
		let auto = shared.auto();
		eff_scale = if auto {
			eff_scale.clamp(AUTO_MIN_SCALE, ceiling)
		} else {
			ceiling
		};
		let force_key = shared.take_force_key();

		// Downscale to an even-dimensioned canvas (4:2:0 needs even w/h). At 100%
		// scale with even source dims this borrows untouched — no per-frame resize.
		let prep_started = Instant::now();
		let scaled = prepare(&raw.rgba, eff_scale);
		let (cw, ch) = (scaled.width(), scaled.height());
		let downscale_us = prep_started.elapsed().as_micros() as u64;

		if enc.is_none() || built != (bitrate, fps, cw, ch) {
			match build_encoder(cw as usize, ch as usize, bitrate, fps) {
				Ok(e) => {
					let kind = e.kind();
					if announced != Some(kind) {
						crate::net::log(&format!("h264 encoder: {kind}"));
						announced = Some(kind);
					}
					shared.report_encoder(kind);
					enc = Some(e);
					built = (bitrate, fps, cw, ch);
					last_hash = None;
				}
				Err(e) => {
					crate::net::log(&format!("h264 encoder init failed: {e:#}"));
					break;
				}
			}
		}

		// Static-frame gate: an identical frame emits nothing, holding an idle
		// screen at zero bandwidth and skipping the encode. A forced keyframe or a
		// dimension change always re-encodes.
		let hash_started = Instant::now();
		let hash = frame_hash(scaled.as_raw());
		let hash_us = hash_started.elapsed().as_micros() as u64;
		if !force_key && last_hash == Some(hash) && last_dims == (cw, ch) {
			stats.record_skip(&raw, downscale_us, hash_us);
			stats.maybe_log(eff_scale, bitrate);
			continue;
		}

		// The backend owns colour conversion (RGBA → I420/NV12) and the encode.
		let encoder = enc.as_mut().expect("encoder built above");
		let enc_started = Instant::now();
		let result = encoder.encode(scaled.as_raw(), cw as usize, ch as usize, force_key);
		let encode_us = enc_started.elapsed().as_micros() as u64;
		// Sub-phase breakdown of that encode (convert/codec/drain), for the stats line.
		let phases = encoder.last_phases();
		last_hash = Some(hash);
		last_dims = (cw, ch);

		let (is_key, au) = match result {
			Ok(Some(v)) => v,
			// The encoder dropped this frame to hold the bitrate — send nothing, like
			// the static gate.
			Ok(None) => {
				stats.record_skip(&raw, downscale_us, hash_us);
				stats.maybe_log(eff_scale, bitrate);
				continue;
			}
			Err(e) => {
				crate::net::log(&format!("h264 encode failed: {e:#}"));
				break;
			}
		};

		seq += 1;
		let body = video::frame_message(is_key, seq, cw, ch, &au);
		let bytes = body.len() as u64;
		let out = OutFrame {
			source_width: raw.width,
			source_height: raw.height,
			origin_x: raw.origin_x,
			origin_y: raw.origin_y,
			body,
		};
		// Backpressure (not frame-dropping): a P-frame must reach the controller or
		// its canvas goes stale, so block here. How long we block measures how far
		// behind the link is.
		let send_started = Instant::now();
		if tx.blocking_send(out).is_err() {
			break;
		}
		let send_us = send_started.elapsed().as_micros() as u64;

		// Adaptive scale reacts to encode/network pressure (a scale change re-inits
		// the encoder, which emits a fresh keyframe on the next frame).
		if auto {
			let interval_us = 1_000_000 / shared.max_fps();
			let behind = encode_us > interval_us || send_us > interval_us;
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

		stats.record(&raw, downscale_us, hash_us, encode_us, phases, send_us, bytes, is_key);
		stats.maybe_log(eff_scale, bitrate);
	}
}

/// A live H.264 encoder backend. Implementations own their codec state and any
/// colour-conversion scratch buffers; [`run`] drives them with already-scaled,
/// even-dimensioned RGBA frames. Each instance is built for a fixed resolution —
/// `run` rebuilds it when the coded dimensions (or bitrate/fps) change, so a
/// rebuild is also where a fresh keyframe naturally falls.
pub(crate) trait ScreenEncoder {
	/// Encode one tightly-packed RGBA frame at `w`×`h` (both even). Returns the
	/// Annex-B access unit plus whether it's a keyframe (IDR), or `None` when the
	/// encoder produced no output (a frame it dropped to hold the bitrate).
	/// `force_key` requests an IDR for this frame.
	fn encode(&mut self, rgba: &[u8], w: usize, h: usize, force_key: bool) -> anyhow::Result<Option<(bool, Vec<u8>)>>;

	/// Human-readable backend name, logged once per session so an operator can see
	/// which encoder is actually running (software vs hardware).
	fn kind(&self) -> &'static str;

	/// Sub-phase timing of the most recent [`Self::encode`] call, in microseconds:
	/// `(convert, core, drain)` — colour conversion, the codec call (Media Foundation
	/// `ProcessInput`), and output collection (`ProcessOutput`/drain). Lets the stats
	/// line show *where* the encode budget goes. Backends that don't split it (software
	/// OpenH264) return zeros — their whole cost shows as `enc`.
	fn last_phases(&self) -> (u64, u64, u64) {
		(0, 0, 0)
	}
}

/// Env var selecting the encoder backend at runtime (no rebuild): `software`,
/// `hardware`, or unset/`auto` for [`DEFAULT_ENCODER_PREF`].
const ENCODER_ENV: &str = "LIBRETETHER_ENCODER";

/// The backend used when [`ENCODER_ENV`] is unset (or `auto`). **Software today**,
/// while the Windows Media Foundation encoder is runtime-unvalidated: it's compiled
/// into every Windows agent but only used when explicitly requested with
/// `LIBRETETHER_ENCODER=hardware`, so an untested `unsafe` codepath can't run on a
/// production guest by accident. Once it's confirmed on real GPUs, flip this one
/// constant to [`EncoderPref::Hardware`] and every agent prefers it by default (still
/// falling back to software on a guest with no usable encoder).
const DEFAULT_ENCODER_PREF: EncoderPref = EncoderPref::Software;

/// Which H.264 backend to use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EncoderPref {
	/// Software OpenH264 — the cross-platform backend, and the fallback everywhere.
	Software,
	/// Prefer the platform's hardware encoder (Media Foundation on Windows); falls
	/// back to software if it can't initialise or the platform has none.
	Hardware,
}

/// Parse [`ENCODER_ENV`]. Unset, `auto`, or anything unrecognised → the current
/// [`DEFAULT_ENCODER_PREF`], so a typo never silently forces a backend.
fn encoder_pref_from(value: Option<&str>) -> EncoderPref {
	match value.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
		Some("software") | Some("sw") | Some("openh264") => EncoderPref::Software,
		Some("hardware") | Some("hw") | Some("mf") | Some("media-foundation") => EncoderPref::Hardware,
		_ => DEFAULT_ENCODER_PREF,
	}
}

/// Pick the backend, honouring [`ENCODER_ENV`]. The Windows Media Foundation encoder
/// is compiled into every Windows agent but only used when explicitly selected (it's
/// still runtime-unvalidated — see [`crate::mf_encoder`]); everything else uses
/// software OpenH264. Both emit the same H.264 wire format, so this is a runtime
/// capability choice, not a protocol fallback, and a hardware encoder that won't
/// initialise degrades to software without a version break.
fn build_encoder(width: usize, height: usize, bitrate_kbps: u32, fps: u8) -> anyhow::Result<Box<dyn ScreenEncoder>> {
	if encoder_pref_from(std::env::var(ENCODER_ENV).ok().as_deref()) == EncoderPref::Hardware {
		#[cfg(windows)]
		match crate::mf_encoder::MediaFoundationEncoder::new(width, height, bitrate_kbps, fps) {
			// `run` announces the chosen backend via `kind()`; here we only note the
			// notable case where hardware was asked for but couldn't be used.
			Ok(enc) => return Ok(Box::new(enc)),
			Err(e) => crate::net::log(&format!(
				"h264: Media Foundation unavailable ({e:#}); falling back to software OpenH264"
			)),
		}
		#[cfg(not(windows))]
		crate::net::log(
			"h264: a hardware encoder was requested but none is available on this platform yet; using software OpenH264",
		);
	}
	Ok(Box::new(OpenH264Encoder::new(width, height, bitrate_kbps, fps)?))
}

/// Software H.264 via OpenH264 — the cross-platform backend and the Windows
/// fallback. Owns the encoder plus a reusable I420 conversion buffer.
struct OpenH264Encoder {
	enc: Encoder,
	yuv: I420,
	width: usize,
	height: usize,
}

impl OpenH264Encoder {
	fn new(width: usize, height: usize, bitrate_kbps: u32, fps: u8) -> anyhow::Result<Self> {
		// Baseline profile (no B-frames, CAVLC) for the widest WebCodecs decode
		// support, including WebKitGTK; rate control sized to the target bitrate.
		let config = EncoderConfig::new()
			.usage_type(UsageType::ScreenContentRealTime)
			.rate_control_mode(RateControlMode::Bitrate)
			.bitrate(BitRate::from_bps(bitrate_kbps.saturating_mul(1000)))
			.max_frame_rate(FrameRate::from_hz(fps.max(1) as f32))
			.profile(Profile::Baseline)
			.vui(VuiConfig::bt601())
			// Frame-skip must be on for bitrate-mode rate control to actually hold the
			// target — when motion outpaces the budget the encoder drops a frame (an
			// empty access unit we forward as nothing), the right degradation under a
			// bitrate cap. Steady-state static frames never reach the encoder anyway:
			// the whole-frame hash gate short-circuits them first.
			.skip_frames(true)
			// Not supported in screen-content mode (OpenH264 auto-disables them and
			// warns); set explicitly to keep the logs clean.
			.adaptive_quantization(false)
			.background_detection(false)
			.num_threads(1);
		let enc = Encoder::with_api_config(OpenH264API::from_source(), config)
			.map_err(|e| anyhow::anyhow!("openh264 init: {e}"))?;
		Ok(Self {
			enc,
			yuv: I420::new(width, height),
			width,
			height,
		})
	}
}

impl ScreenEncoder for OpenH264Encoder {
	fn encode(&mut self, rgba: &[u8], w: usize, h: usize, force_key: bool) -> anyhow::Result<Option<(bool, Vec<u8>)>> {
		debug_assert_eq!(
			(w, h),
			(self.width, self.height),
			"frame size must match the built encoder"
		);
		if force_key {
			self.enc.force_intra_frame();
		}
		// RGBA → I420 (BT.601 limited range; signalled via the encoder's VUI).
		rgba_to_i420(rgba, w, h, &mut self.yuv);
		let slices = self.yuv.as_slices();
		let bs = self
			.enc
			.encode(&slices)
			.map_err(|e| anyhow::anyhow!("openh264 encode: {e}"))?;
		let au = bs.to_vec();
		if au.is_empty() {
			return Ok(None);
		}
		Ok(Some((matches!(bs.frame_type(), FrameType::IDR | FrameType::I), au)))
	}

	fn kind(&self) -> &'static str {
		"OpenH264 (software)"
	}
}

/// Downscale by `scale` percent to even dimensions, borrowing the original
/// untouched when it already matches (the common 100%/even case). 4:2:0 requires
/// even width and height, so odd targets are rounded down.
fn prepare(img: &RgbaImage, scale: u8) -> Cow<'_, RgbaImage> {
	let (w, h) = (img.width(), img.height());
	let tw = even(((w as u64 * scale as u64) / 100) as u32).max(2);
	let th = even(((h as u64 * scale as u64) / 100) as u32).max(2);
	if tw == w && th == h {
		Cow::Borrowed(img)
	} else {
		Cow::Owned(imageops::resize(img, tw, th, FilterType::Triangle))
	}
}

#[inline]
fn even(x: u32) -> u32 {
	x & !1
}

/// A reusable I420 (YUV 4:2:0 planar) buffer: `Y` (w×h) then `U` and `V`
/// (w/2×h/2 each) packed contiguously.
struct I420 {
	data: Vec<u8>,
	w: usize,
	h: usize,
}

impl I420 {
	fn new(w: usize, h: usize) -> Self {
		let (cw, ch) = (w / 2, h / 2);
		Self {
			data: vec![0u8; w * h + 2 * cw * ch],
			w,
			h,
		}
	}

	fn ensure(&mut self, w: usize, h: usize) {
		if self.w != w || self.h != h {
			*self = I420::new(w, h);
		}
	}

	/// Borrow the three planes as an OpenH264 `YUVSource`.
	fn as_slices(&self) -> YUVSlices<'_> {
		let (cw, ch) = (self.w / 2, self.h / 2);
		let (y, uv) = self.data.split_at(self.w * self.h);
		let (u, v) = uv.split_at(cw * ch);
		YUVSlices::new((y, u, v), (self.w, self.h), (self.w, cw, cw))
	}
}

/// Convert tightly-packed RGBA into `dst` as I420, BT.601 limited range. The Y
/// plane parallelizes over rows; chroma over 2×2-block rows (each averaging a
/// 2×2 quad). `w` and `h` must be even.
fn rgba_to_i420(rgba: &[u8], w: usize, h: usize, dst: &mut I420) {
	dst.ensure(w, h);
	let (cw, ch) = (w / 2, h / 2);
	let (y_plane, uv) = dst.data.split_at_mut(w * h);
	let (u_plane, v_plane) = uv.split_at_mut(cw * ch);

	y_plane.par_chunks_mut(w).enumerate().for_each(|(row, yrow)| {
		let base = row * w * 4;
		for (x, y) in yrow.iter_mut().enumerate() {
			let p = base + x * 4;
			let (r, g, b) = (rgba[p] as i32, rgba[p + 1] as i32, rgba[p + 2] as i32);
			*y = clamp8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16);
		}
	});

	let _ = ch;
	u_plane
		.par_chunks_mut(cw)
		.zip(v_plane.par_chunks_mut(cw))
		.enumerate()
		.for_each(|(crow, (urow, vrow))| {
			let y0 = crow * 2;
			for cx in 0..cw {
				let x0 = cx * 2;
				let (mut r, mut g, mut b) = (0i32, 0i32, 0i32);
				for dy in 0..2 {
					let row_base = (y0 + dy) * w * 4;
					for dx in 0..2 {
						let p = row_base + (x0 + dx) * 4;
						r += rgba[p] as i32;
						g += rgba[p + 1] as i32;
						b += rgba[p + 2] as i32;
					}
				}
				r >>= 2;
				g >>= 2;
				b >>= 2;
				urow[cx] = clamp8(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128);
				vrow[cx] = clamp8(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128);
			}
		});
}

#[inline]
fn clamp8(v: i32) -> u8 {
	v.clamp(0, 255) as u8
}

/// Convert tightly-packed RGBA into `dst` as **NV12** (BT.601 limited range): a
/// full-res Y plane followed by a half-res plane of interleaved U/V. NV12 is I420
/// with the two chroma planes interleaved, so this stays in step with [`rgba_to_i420`]
/// byte for byte (the test locks that in). Used by the Windows Media Foundation
/// encoder, whose MFTs want NV12 input; kept here (not in the Windows-only
/// `mf_encoder`) so it's compiled and tested on every platform. `w`/`h` must be even.
// Only the Windows `mf_encoder` (and the tests) call this, so it's unused on other
// build configs — that's expected, not dead code.
#[allow(dead_code)]
pub(crate) fn rgba_to_nv12(rgba: &[u8], w: usize, h: usize, dst: &mut [u8]) {
	let (y_plane, uv_plane) = dst.split_at_mut(w * h);
	// Y plane: iterate row/pixel slices with `chunks_exact` so the bounds checks are
	// elided and the inner loop autovectorizes — this pass is ~w·h iterations and was
	// the dominant CPU cost of the encode path. Same BT.601 integer math as before.
	for (row_rgba, row_y) in rgba.chunks_exact(w * 4).zip(y_plane.chunks_exact_mut(w)) {
		for (px, y) in row_rgba.chunks_exact(4).zip(row_y.iter_mut()) {
			let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
			*y = clamp8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16);
		}
	}
	// Chroma (NV12 interleaved U,V): one 2×2 box-average per output pair. Each chroma
	// row is `w` bytes (w/2 U + w/2 V). Slice the two source rows once so the block
	// reads stay within fixed-length subslices.
	let cw = w / 2;
	for (cy, uv_row) in uv_plane.chunks_exact_mut(w).enumerate() {
		let row0 = &rgba[(cy * 2) * w * 4..(cy * 2 + 1) * w * 4];
		let row1 = &rgba[(cy * 2 + 1) * w * 4..(cy * 2 + 2) * w * 4];
		for cx in 0..cw {
			let o = cx * 8; // 2 px × 4 bytes
			let r = (row0[o] as i32 + row0[o + 4] as i32 + row1[o] as i32 + row1[o + 4] as i32) >> 2;
			let g = (row0[o + 1] as i32 + row0[o + 5] as i32 + row1[o + 1] as i32 + row1[o + 5] as i32) >> 2;
			let b = (row0[o + 2] as i32 + row0[o + 6] as i32 + row1[o + 2] as i32 + row1[o + 6] as i32) >> 2;
			uv_row[cx * 2] = clamp8(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128);
			uv_row[cx * 2 + 1] = clamp8(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128);
		}
	}
}

/// Cheap check: does the Annex-B access unit *start* with an SPS NAL (type 7)? Media
/// Foundation keeps SPS/PPS out of band, so the encoder re-injects them on each
/// keyframe unless they're already there; this guards against double-prepending.
/// Only the first few bytes are inspected — a leading SPS follows a 3- or 4-byte
/// start code.
// As with `rgba_to_nv12`, only the Windows encoder and the tests use this.
#[allow(dead_code)]
pub(crate) fn starts_with_sps(au: &[u8]) -> bool {
	au.windows(3)
		.take(8)
		.enumerate()
		.any(|(i, w)| w == [0, 0, 1] && au.get(i + 3).is_some_and(|b| b & 0x1f == 7))
}

/// FNV-1a over the frame, parallelized in blocks. Used only to detect a frame
/// identical to the last one (skip the encode), so the order-independent block
/// fold is fine — any pixel change flips the result.
fn frame_hash(bytes: &[u8]) -> u64 {
	const BLOCK: usize = 1 << 16;
	bytes
		.par_chunks(BLOCK)
		.enumerate()
		.map(|(i, c)| fnv1a(c).wrapping_add(i as u64).rotate_left((i % 64) as u32))
		.reduce(|| 0xcbf2_9ce4_8422_2325, |a, b| a ^ b)
}

fn fnv1a(bytes: &[u8]) -> u64 {
	let mut h: u64 = 0xcbf2_9ce4_8422_2325;
	let mut chunks = bytes.chunks_exact(8);
	for c in &mut chunks {
		let v = u64::from_le_bytes(c.try_into().unwrap());
		h = (h ^ v).wrapping_mul(0x0000_0100_0000_01b3);
	}
	for &b in chunks.remainder() {
		h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
	}
	h
}

/// Rolling per-stage timing, logged ~once a second while a session runs so the
/// operator can see exactly where the frame budget goes (capture vs downscale vs
/// hash vs encode vs network) on each guest. The `encode` figure covers the
/// backend's colour conversion plus the H.264 encode.
struct Stats {
	window_start: Instant,
	frames: u32,
	sent: u32,
	keyframes: u32,
	capture_us: u64,
	downscale_us: u64,
	hash_us: u64,
	encode_us: u64,
	/// Breakdown of `encode_us`: colour conversion, the codec call, output drain.
	/// Populated by the hardware (Media Foundation) backend; zero for software.
	convert_us: u64,
	submit_us: u64,
	drain_us: u64,
	send_us: u64,
	bytes: u64,
}

impl Stats {
	fn new() -> Self {
		Self {
			window_start: Instant::now(),
			frames: 0,
			sent: 0,
			keyframes: 0,
			capture_us: 0,
			downscale_us: 0,
			hash_us: 0,
			encode_us: 0,
			convert_us: 0,
			submit_us: 0,
			drain_us: 0,
			send_us: 0,
			bytes: 0,
		}
	}

	#[allow(clippy::too_many_arguments)]
	fn record(
		&mut self,
		raw: &RawFrame,
		downscale_us: u64,
		hash_us: u64,
		encode_us: u64,
		phases: (u64, u64, u64),
		send_us: u64,
		bytes: u64,
		key: bool,
	) {
		self.frames += 1;
		self.sent += 1;
		if key {
			self.keyframes += 1;
		}
		self.capture_us += raw.capture_us;
		self.downscale_us += downscale_us;
		self.hash_us += hash_us;
		self.encode_us += encode_us;
		let (convert_us, submit_us, drain_us) = phases;
		self.convert_us += convert_us;
		self.submit_us += submit_us;
		self.drain_us += drain_us;
		self.send_us += send_us;
		self.bytes += bytes;
	}

	/// A frame that produced no output (static gate or encoder skip): it still cost
	/// capture/downscale/hash, so account for those.
	fn record_skip(&mut self, raw: &RawFrame, downscale_us: u64, hash_us: u64) {
		self.frames += 1;
		self.capture_us += raw.capture_us;
		self.downscale_us += downscale_us;
		self.hash_us += hash_us;
	}

	fn maybe_log(&mut self, scale: u8, bitrate: u32) {
		let elapsed = self.window_start.elapsed();
		if elapsed < Duration::from_secs(1) || self.frames == 0 {
			return;
		}
		let n = self.frames as f64;
		let ms = |total: u64| (total as f64 / n) / 1000.0;
		let fps = n / elapsed.as_secs_f64();
		let kib_per_sent = (self.bytes as f64 / self.sent.max(1) as f64) / 1024.0;
		// Per-second telemetry: useful for tuning but too chatty for the default Info
		// view, so it's logged at Debug (the Logs page filters it out by default).
		crate::net::debug(&format!(
			"stream {fps:.0} fps ({}/s sent, {} key) | cap {:.1} down {:.1} hash {:.1} enc {:.1} (conv {:.1} sub {:.1} drn {:.1}) net {:.1} ms/f | {kib_per_sent:.0} KiB/sent | scale {scale}% {bitrate}kbps",
			self.sent,
			self.keyframes,
			ms(self.capture_us),
			ms(self.downscale_us),
			ms(self.hash_us),
			ms(self.encode_us),
			ms(self.convert_us),
			ms(self.submit_us),
			ms(self.drain_us),
			ms(self.send_us),
		));
		*self = Stats::new();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn solid(w: u32, h: u32, px: [u8; 4]) -> RgbaImage {
		RgbaImage::from_pixel(w, h, image::Rgba(px))
	}

	#[test]
	fn prepare_rounds_to_even_and_borrows_at_native() {
		// 100% with even dims borrows untouched.
		let img = solid(200, 100, [0, 0, 0, 255]);
		assert!(matches!(prepare(&img, 100), Cow::Borrowed(_)));
		// Downscale halves dimensions (and stays even).
		let small = prepare(&img, 50);
		assert_eq!((small.width(), small.height()), (100, 50));
		// Odd target dims are rounded down to even.
		let odd = solid(101, 51, [0, 0, 0, 255]);
		let prepped = prepare(&odd, 100);
		assert_eq!((prepped.width(), prepped.height()), (100, 50));
	}

	#[test]
	fn rgba_to_i420_fills_expected_plane_sizes() {
		let img = solid(4, 2, [255, 255, 255, 255]);
		let mut yuv = I420::new(2, 2);
		rgba_to_i420(img.as_raw(), 4, 2, &mut yuv);
		assert_eq!(yuv.w, 4);
		assert_eq!(yuv.h, 2);
		// Y (4×2) + U (2×1) + V (2×1) = 8 + 2 + 2.
		assert_eq!(yuv.data.len(), 8 + 2 + 2);
		// White → Y near 235 (limited-range peak).
		assert!(yuv.data[0] > 230, "white luma should be near 235, got {}", yuv.data[0]);
	}

	/// A colourful gradient so every U/V pair differs — a solid image would hide a
	/// U/V transposition or interleave bug.
	fn gradient(w: u32, h: u32) -> RgbaImage {
		RgbaImage::from_fn(w, h, |x, y| {
			image::Rgba([(x * 17) as u8, (y * 29) as u8, (x * y * 3) as u8, 255])
		})
	}

	// NV12 must be exactly I420 with the two chroma planes interleaved. Deriving both
	// from the same input and comparing locks the Windows encoder's colour conversion
	// to the already-tested I420 path — catching a swapped U/V or a bad stride, which
	// would otherwise only surface as wrong colours on a real Windows guest.
	#[test]
	fn nv12_is_i420_with_interleaved_chroma() {
		let (w, h) = (8usize, 6usize);
		let img = gradient(w as u32, h as u32);

		let mut i420 = I420::new(w, h);
		rgba_to_i420(img.as_raw(), w, h, &mut i420);
		let (cw, ch) = (w / 2, h / 2);
		let (y_i, uv_i) = i420.data.split_at(w * h);
		let (u_i, v_i) = uv_i.split_at(cw * ch);

		let mut nv12 = vec![0u8; w * h + 2 * cw * ch];
		rgba_to_nv12(img.as_raw(), w, h, &mut nv12);
		let (y_n, uv_n) = nv12.split_at(w * h);

		assert_eq!(y_n, y_i, "Y planes must be identical");
		for cy in 0..ch {
			for cx in 0..cw {
				assert_eq!(uv_n[cy * w + cx * 2], u_i[cy * cw + cx], "U at ({cx},{cy})");
				assert_eq!(uv_n[cy * w + cx * 2 + 1], v_i[cy * cw + cx], "V at ({cx},{cy})");
			}
		}
	}

	#[test]
	fn starts_with_sps_detects_only_a_leading_sps() {
		// 4-byte start code + SPS (NAL type 7).
		assert!(starts_with_sps(&[0, 0, 0, 1, 0x67, 0x42]));
		// 3-byte start code + SPS.
		assert!(starts_with_sps(&[0, 0, 1, 0x67]));
		// Leading P-slice (NAL type 1) — no SPS.
		assert!(!starts_with_sps(&[0, 0, 0, 1, 0x41, 0x9a]));
		// Empty / too short never matches.
		assert!(!starts_with_sps(&[]));
		assert!(!starts_with_sps(&[0, 0, 1]));
	}

	#[test]
	fn encoder_pref_parses_and_defaults_to_software() {
		// Unset / `auto` / anything unrecognised → the current default, never a silent
		// forced backend. The default stays software while the hardware path is
		// unvalidated — this assertion is the guard on that.
		assert_eq!(
			DEFAULT_ENCODER_PREF,
			EncoderPref::Software,
			"hardware must stay opt-in until validated"
		);
		assert_eq!(encoder_pref_from(None), DEFAULT_ENCODER_PREF);
		assert_eq!(encoder_pref_from(Some("auto")), DEFAULT_ENCODER_PREF);
		assert_eq!(encoder_pref_from(Some("gpu")), DEFAULT_ENCODER_PREF);
		for sw in ["software", "SW", " OpenH264 "] {
			assert_eq!(encoder_pref_from(Some(sw)), EncoderPref::Software, "{sw:?}");
		}
		for hw in ["hardware", "hw", "mf", "Media-Foundation"] {
			assert_eq!(encoder_pref_from(Some(hw)), EncoderPref::Hardware, "{hw:?}");
		}
	}

	#[test]
	fn static_frame_hash_is_stable_and_change_sensitive() {
		let a = solid(16, 16, [10, 20, 30, 255]);
		let mut b = a.clone();
		assert_eq!(frame_hash(a.as_raw()), frame_hash(b.as_raw()));
		b.put_pixel(3, 3, image::Rgba([0, 0, 0, 255]));
		assert_ne!(frame_hash(a.as_raw()), frame_hash(b.as_raw()));
	}

	/// Does the Annex-B access unit contain an SPS NAL (type 7)? Mirrors the
	/// controller's `avcCodecFromKeyframe`, which reads the codec string from it.
	fn has_sps(au: &[u8]) -> bool {
		au.windows(3)
			.enumerate()
			.any(|(i, w)| w == [0, 0, 1] && au.get(i + 3).is_some_and(|b| b & 0x1f == 7))
	}

	#[test]
	fn first_frame_is_an_idr_then_deltas_follow() {
		let mut enc = OpenH264Encoder::new(64, 64, 4_000, 30).expect("encoder builds");
		let frame = solid(64, 64, [120, 130, 140, 255]);

		let (key, au) = enc
			.encode(frame.as_raw(), 64, 64, false)
			.unwrap()
			.expect("keyframe emitted");
		assert!(key, "first frame must be a keyframe");
		assert!(
			has_sps(&au),
			"keyframe must carry an in-band SPS so the decoder can configure"
		);

		// A second, slightly different frame should still produce output (a P-frame).
		let mut moved = frame.clone();
		moved.put_pixel(10, 10, image::Rgba([0, 0, 0, 255]));
		let (_, au2) = enc
			.encode(moved.as_raw(), 64, 64, false)
			.unwrap()
			.expect("delta emitted");
		assert!(!au2.is_empty());
	}
}
