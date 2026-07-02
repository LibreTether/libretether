//! Windows screen capture via DXGI Desktop Duplication, with a GDI fallback.
//!
//! Replaces xcap's per-frame GDI `BitBlt` (which measured ~40 ms/frame — the
//! dominant cost on Windows, capping the stream around 25 fps regardless of the
//! encoder) with a persistent, GPU-accelerated output duplication. It only wakes
//! when the desktop actually changes (`AcquireNextFrame` blocks until then), so an
//! idle screen costs nothing and an active one runs at the compositor's rate.
//!
//! The `windows`-crate calls mirror xcap's proven video-recorder setup, with the
//! differences a long-lived control session needs: a real stop flag (the output
//! duplication is released on teardown — only one can exist per output at a time,
//! so a leak would break the next session), automatic rebuild on `ACCESS_LOST`
//! (desktop switch / resolution change / GPU reset), a frame-rate gate, and
//! row-pitch-correct conversion to tight RGBA.
//!
//! Two things keep the per-frame cost down: the CPU-readable **staging texture is
//! reused** across frames (rather than allocating a GPU resource every capture), and
//! on a mostly-static screen only the **dirty rectangles** DXGI reports are copied off
//! the GPU and converted — the rest of the frame carries over from the previous one in
//! an in-memory accumulator. A frame with a scroll/blit (a DXGI *move* rect), or one
//! whose changed area is more than half the screen, falls back to a full-frame copy.
//!
//! Desktop Duplication needs a usable display adapter, which some contexts don't
//! provide (an RDP/console-0 session, a GPU-less VM). When duplication can't be
//! created we fall back to [`crate::capture::poll_loop`] — xcap's GDI `BitBlt`,
//! slower but available wherever the old path was.

use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use image::RgbaImage;
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
	D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_BOX,
	D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_SINGLETHREADED,
	D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{
	IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
	DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT,
};

use crate::encode::{RawFrame, SharedConfig};

/// Consecutive duplication-setup failures before we conclude DXGI isn't available
/// in this environment and fall back to GDI for the rest of the session.
const MAX_SETUP_FAILURES: u32 = 3;

/// Spawn the Windows capture thread. Returns immediately; the thread exits when
/// `stop` is set.
pub fn spawn(
	display: u32,
	shared: Arc<SharedConfig>,
	stop: Arc<AtomicBool>,
	tx: SyncSender<RawFrame>,
) -> std::thread::JoinHandle<()> {
	std::thread::spawn(move || capture_loop(display, shared, stop, tx))
}

fn capture_loop(display: u32, shared: Arc<SharedConfig>, stop: Arc<AtomicBool>, tx: SyncSender<RawFrame>) {
	let mut setup_failures = 0u32;
	// Once duplication has worked, later setup failures are transient (e.g. the
	// secure desktop during a UAC prompt) — keep retrying DXGI rather than
	// downgrading the rest of the session to GDI.
	let mut ever_succeeded = false;
	while !stop.load(Ordering::Relaxed) {
		match unsafe { setup(display) } {
			Ok(dxgi) => {
				ever_succeeded = true;
				setup_failures = 0;
				shared.report_capture("DXGI");
				match unsafe { run_frames(&dxgi, &shared, &stop, &tx) } {
					Ok(Outcome::Stopped) => return,
					// `dxgi` drops here (releasing the duplication) before we rebuild.
					Ok(Outcome::Lost) => continue,
					Err(e) => {
						crate::net::log(&format!("dxgi capture error: {e}; rebuilding"));
						continue;
					}
				}
			}
			Err(e) => {
				// Only conclude DXGI is genuinely unavailable (no GPU, an RDP /
				// console-0 session) if it never once worked.
				if !ever_succeeded {
					setup_failures += 1;
					if setup_failures >= MAX_SETUP_FAILURES {
						crate::net::log(&format!(
							"DXGI duplication unavailable ({e}); falling back to GDI capture"
						));
						shared.report_capture("GDI");
						crate::capture::poll_loop(display, shared, stop, tx);
						return;
					}
				}
				crate::net::log(&format!("dxgi setup failed ({e}); retrying"));
				std::thread::sleep(Duration::from_millis(300));
			}
		}
	}
}

/// A live output duplication plus the device that owns it. Dropping it releases
/// the duplication (only one per output may exist at a time).
struct Dxgi {
	device: ID3D11Device,
	context: ID3D11DeviceContext,
	duplication: IDXGIOutputDuplication,
	origin_x: i32,
	origin_y: i32,
}

/// Why a frame loop ended.
enum Outcome {
	/// The session is shutting down (stop flag set or the encoder hung up).
	Stopped,
	/// The duplication was lost (desktop switch / mode change) — rebuild it.
	Lost,
}

unsafe fn setup(display: u32) -> Result<Dxgi> {
	let mut device: Option<ID3D11Device> = None;
	let mut context: Option<ID3D11DeviceContext> = None;
	D3D11CreateDevice(
		None,
		D3D_DRIVER_TYPE_HARDWARE,
		HMODULE::default(),
		D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_SINGLETHREADED,
		None,
		D3D11_SDK_VERSION,
		Some(&mut device),
		None,
		Some(&mut context),
	)?;
	let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?;
	let context = context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?;

	let dxgi_device: IDXGIDevice = device.cast()?;
	let adapter = dxgi_device.GetAdapter()?;
	let output = adapter.EnumOutputs(display)?;
	let output_desc = output.GetDesc()?;
	let output1: IDXGIOutput1 = output.cast()?;
	let duplication = output1.DuplicateOutput(&device)?;

	Ok(Dxgi {
		device,
		context,
		duplication,
		origin_x: output_desc.DesktopCoordinates.left,
		origin_y: output_desc.DesktopCoordinates.top,
	})
}

/// A CPU-readable staging texture, kept alive across frames so we don't allocate a
/// GPU resource on every capture (creating one per frame was pure per-frame overhead).
/// Rebuilt only when the frame dimensions change.
struct Staging {
	texture: ID3D11Texture2D,
	width: u32,
	height: u32,
}

/// The last full frame in tight RGBA, kept so a mostly-static screen can be updated
/// in place from just the DXGI dirty rectangles instead of copying + converting the
/// whole frame every time.
struct Frame {
	rgba: Vec<u8>,
	width: u32,
	height: u32,
}

unsafe fn run_frames(
	dxgi: &Dxgi,
	shared: &Arc<SharedConfig>,
	stop: &Arc<AtomicBool>,
	tx: &SyncSender<RawFrame>,
) -> Result<Outcome> {
	let mut last_emit: Option<Instant> = None;
	// Per-session capture scratch, reused across frames (see `Staging`/`Frame`).
	let mut staging: Option<Staging> = None;
	let mut acc: Option<Frame> = None;
	let mut move_buf: Vec<DXGI_OUTDUPL_MOVE_RECT> = Vec::new();
	let mut dirty_buf: Vec<RECT> = Vec::new();
	loop {
		if stop.load(Ordering::Relaxed) {
			return Ok(Outcome::Stopped);
		}
		let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
		let mut resource: Option<IDXGIResource> = None;
		let started = Instant::now();
		match dxgi.duplication.AcquireNextFrame(200, &mut info, &mut resource) {
			Ok(()) => {}
			// No change within the timeout — loop (re-checks the stop flag); nothing
			// was acquired, so there's no frame to release.
			Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
			Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Ok(Outcome::Lost),
			Err(e) => return Err(e.into()),
		}

		// `LastPresentTime == 0` means only the pointer moved (the cursor isn't part
		// of the duplicated image), so there's nothing new to encode.
		let present = info.LastPresentTime != 0;
		let interval = Duration::from_millis(1000 / shared.max_fps());
		let due = last_emit.is_none_or(|t| t.elapsed() >= interval);

		if present && due {
			if let Some(resource) = resource.as_ref() {
				let texture: ID3D11Texture2D = resource.cast()?;
				match capture_frame(
					dxgi,
					&texture,
					&info,
					&mut staging,
					&mut acc,
					&mut move_buf,
					&mut dirty_buf,
				) {
					Ok((width, height, rgba)) => {
						let image =
							RgbaImage::from_raw(width, height, rgba).ok_or_else(|| anyhow!("rgba size mismatch"))?;
						let raw = RawFrame {
							width,
							height,
							origin_x: dxgi.origin_x,
							origin_y: dxgi.origin_y,
							rgba: image,
							capture_us: started.elapsed().as_micros() as u64,
							// DXGI only wakes on an actual present, so this frame changed —
							// the encoder can skip its dedup hash.
							pre_deduped: true,
						};
						match tx.try_send(raw) {
							Ok(()) => last_emit = Some(Instant::now()),
							// Encoder busy — drop, stay real-time.
							Err(TrySendError::Full(_)) => {}
							Err(TrySendError::Disconnected(_)) => {
								let _ = dxgi.duplication.ReleaseFrame();
								return Ok(Outcome::Stopped);
							}
						}
					}
					Err(e) => crate::net::log(&format!("dxgi map failed: {e}")),
				}
			}
		}
		dxgi.duplication.ReleaseFrame()?;
	}
}

/// Produce this frame as tight RGBA. When the accumulator holds a same-size previous
/// frame and the change is a small, move-free set of dirty rectangles, only those
/// rectangles are copied off the GPU and converted (updating the accumulator in
/// place); otherwise the whole frame is copied + converted. Either way the staging
/// texture is reused, and a snapshot of the accumulator is returned to send.
unsafe fn capture_frame(
	dxgi: &Dxgi,
	src: &ID3D11Texture2D,
	info: &DXGI_OUTDUPL_FRAME_INFO,
	staging: &mut Option<Staging>,
	acc: &mut Option<Frame>,
	move_buf: &mut Vec<DXGI_OUTDUPL_MOVE_RECT>,
	dirty_buf: &mut Vec<RECT>,
) -> Result<(u32, u32, Vec<u8>)> {
	let mut desc = D3D11_TEXTURE2D_DESC::default();
	src.GetDesc(&mut desc);
	let (w, h) = (desc.Width, desc.Height);
	ensure_staging(&dxgi.device, staging, &desc)?;
	let staging_tex = &staging.as_ref().unwrap().texture;
	let dst_res: ID3D11Resource = staging_tex.cast()?;
	let src_res: ID3D11Resource = src.cast()?;

	// Incremental only when we have a matching-size accumulator and a small, move-free
	// dirty set; otherwise copy + convert the whole frame.
	let dirty = if acc.as_ref().is_some_and(|f| f.width == w && f.height == h) {
		collect_dirty(&dxgi.duplication, info, move_buf, dirty_buf, w, h)
	} else {
		None
	};
	let incremental = dirty
		.as_ref()
		.is_some_and(|d| !d.is_empty() && dirty_area(d) * 2 < w as u64 * h as u64);

	if incremental {
		let rects = dirty.unwrap();
		// Copy only the changed regions off the GPU.
		for r in &rects {
			let b = D3D11_BOX {
				left: r.left as u32,
				top: r.top as u32,
				front: 0,
				right: r.right as u32,
				bottom: r.bottom as u32,
				back: 1,
			};
			dxgi.context.CopySubresourceRegion(
				Some(&dst_res),
				0,
				r.left as u32,
				r.top as u32,
				0,
				Some(&src_res),
				0,
				Some(&b as *const D3D11_BOX),
			);
		}
		let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
		dxgi.context
			.Map(Some(&dst_res), 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
		let pitch = mapped.RowPitch as usize;
		let bytes = slice::from_raw_parts(mapped.pData as *const u8, h as usize * pitch);
		let frame = acc.as_mut().unwrap();
		for r in &rects {
			convert_rect(bytes, pitch, &mut frame.rgba, w as usize, r);
		}
		dxgi.context.Unmap(Some(&dst_res), 0);
		Ok((w, h, frame.rgba.clone()))
	} else {
		// Full frame: one GPU copy, then convert the whole thing into the accumulator.
		dxgi.context.CopyResource(Some(&dst_res), Some(&src_res));
		let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
		dxgi.context
			.Map(Some(&dst_res), 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
		let pitch = mapped.RowPitch as usize;
		let bytes = slice::from_raw_parts(mapped.pData as *const u8, h as usize * pitch);
		if !acc.as_ref().is_some_and(|f| f.width == w && f.height == h) {
			*acc = Some(Frame {
				rgba: vec![0u8; w as usize * h as usize * 4],
				width: w,
				height: h,
			});
		}
		let frame = acc.as_mut().unwrap();
		convert_full(bytes, pitch, &mut frame.rgba, w as usize, h as usize);
		dxgi.context.Unmap(Some(&dst_res), 0);
		Ok((w, h, frame.rgba.clone()))
	}
}

/// Ensure a reusable CPU-readable staging texture matching `desc` exists, rebuilding
/// only when the dimensions change.
unsafe fn ensure_staging(
	device: &ID3D11Device,
	staging: &mut Option<Staging>,
	desc: &D3D11_TEXTURE2D_DESC,
) -> Result<()> {
	if staging
		.as_ref()
		.is_some_and(|s| s.width == desc.Width && s.height == desc.Height)
	{
		return Ok(());
	}
	let mut sdesc = *desc;
	sdesc.BindFlags = 0;
	sdesc.MiscFlags = 0;
	sdesc.Usage = D3D11_USAGE_STAGING;
	sdesc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
	let mut tex: Option<ID3D11Texture2D> = None;
	device.CreateTexture2D(&sdesc, None, Some(&mut tex))?;
	*staging = Some(Staging {
		texture: tex.ok_or_else(|| anyhow!("CreateTexture2D returned no texture"))?,
		width: desc.Width,
		height: desc.Height,
	});
	Ok(())
}

/// Collect this frame's dirty rectangles (clamped to the frame). Returns `None` — a
/// signal to copy the whole frame — if there are any *move* rects (a scroll/blit; the
/// moved pixels are already in the texture, so a full copy is simplest and correct) or
/// if the metadata can't be read.
unsafe fn collect_dirty(
	dup: &IDXGIOutputDuplication,
	info: &DXGI_OUTDUPL_FRAME_INFO,
	move_buf: &mut Vec<DXGI_OUTDUPL_MOVE_RECT>,
	dirty_buf: &mut Vec<RECT>,
	w: u32,
	h: u32,
) -> Option<Vec<RECT>> {
	let total = info.TotalMetadataBufferSize as usize;
	if total == 0 {
		return Some(Vec::new());
	}
	let move_sz = std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
	if move_buf.len() * move_sz < total {
		move_buf.resize(total / move_sz + 1, DXGI_OUTDUPL_MOVE_RECT::default());
	}
	let mut move_bytes = 0u32;
	dup.GetFrameMoveRects(
		(move_buf.len() * move_sz) as u32,
		move_buf.as_mut_ptr(),
		&mut move_bytes,
	)
	.ok()?;
	if move_bytes > 0 {
		return None; // a move/scroll happened — take the full-frame path
	}

	let rect_sz = std::mem::size_of::<RECT>();
	if dirty_buf.len() * rect_sz < total {
		dirty_buf.resize(total / rect_sz + 1, RECT::default());
	}
	let mut dirty_bytes = 0u32;
	dup.GetFrameDirtyRects(
		(dirty_buf.len() * rect_sz) as u32,
		dirty_buf.as_mut_ptr(),
		&mut dirty_bytes,
	)
	.ok()?;
	let n = dirty_bytes as usize / rect_sz;
	Some(dirty_buf[..n].iter().filter_map(|r| clamp_rect(*r, w, h)).collect())
}

/// Clamp a rectangle to `[0,w]×[0,h]`, dropping it if it's empty after clamping.
fn clamp_rect(r: RECT, w: u32, h: u32) -> Option<RECT> {
	let left = r.left.clamp(0, w as i32);
	let top = r.top.clamp(0, h as i32);
	let right = r.right.clamp(0, w as i32);
	let bottom = r.bottom.clamp(0, h as i32);
	(right > left && bottom > top).then_some(RECT {
		left,
		top,
		right,
		bottom,
	})
}

/// Total pixel area of the dirty rectangles (overlaps double-count — a safe overestimate
/// that only biases toward the full-frame path).
fn dirty_area(rects: &[RECT]) -> u64 {
	rects
		.iter()
		.map(|r| (r.right - r.left) as u64 * (r.bottom - r.top) as u64)
		.sum()
}

/// Convert the whole staging map (BGRA, row-pitched) into tight RGBA.
fn convert_full(src: &[u8], pitch: usize, dst: &mut [u8], w: usize, h: usize) {
	for y in 0..h {
		convert_row(&src[y * pitch..], &mut dst[y * w * 4..], 0, w);
	}
}

/// Convert one dirty rectangle from the staging map into the accumulator in place.
fn convert_rect(src: &[u8], pitch: usize, dst: &mut [u8], dst_w: usize, r: &RECT) {
	let (l, rt) = (r.left as usize, r.right as usize);
	for y in r.top as usize..r.bottom as usize {
		convert_row(&src[y * pitch..], &mut dst[y * dst_w * 4..], l, rt);
	}
}

/// Convert pixels `[from, to)` of one row: BGRA (DXGI B8G8R8A8) → opaque RGBA.
fn convert_row(src_row: &[u8], dst_row: &mut [u8], from: usize, to: usize) {
	let src = &src_row[from * 4..to * 4];
	let dst = &mut dst_row[from * 4..to * 4];
	for (px, out) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
		out[0] = px[2];
		out[1] = px[1];
		out[2] = px[0];
		out[3] = 255;
	}
}
