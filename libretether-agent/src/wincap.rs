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
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
	D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
	D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_SINGLETHREADED, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
	D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{
	IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
	DXGI_OUTDUPL_FRAME_INFO,
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

unsafe fn run_frames(
	dxgi: &Dxgi,
	shared: &Arc<SharedConfig>,
	stop: &Arc<AtomicBool>,
	tx: &SyncSender<RawFrame>,
) -> Result<Outcome> {
	let mut last_emit: Option<Instant> = None;
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
				match texture_to_rgba(&dxgi.device, &dxgi.context, &texture) {
					Ok((width, height, rgba)) => {
						let raw = RawFrame {
							width,
							height,
							origin_x: dxgi.origin_x,
							origin_y: dxgi.origin_y,
							rgba,
							capture_us: started.elapsed().as_micros() as u64,
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

/// Copy the duplicated GPU texture into CPU memory and convert BGRA → tight RGBA,
/// stripping the staging texture's row pitch (which is ≥ width·4).
unsafe fn texture_to_rgba(
	device: &ID3D11Device,
	context: &ID3D11DeviceContext,
	src: &ID3D11Texture2D,
) -> Result<(u32, u32, RgbaImage)> {
	let mut desc = D3D11_TEXTURE2D_DESC::default();
	src.GetDesc(&mut desc);
	// A CPU-readable copy of the (GPU-only) duplicated frame.
	desc.BindFlags = 0;
	desc.MiscFlags = 0;
	desc.Usage = D3D11_USAGE_STAGING;
	desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;

	let mut staging: Option<ID3D11Texture2D> = None;
	device.CreateTexture2D(&desc, None, Some(&mut staging))?;
	let staging = staging.ok_or_else(|| anyhow!("CreateTexture2D returned no texture"))?;

	let dst_res: ID3D11Resource = staging.cast()?;
	let src_res: ID3D11Resource = src.cast()?;
	context.CopyResource(Some(&dst_res), Some(&src_res));

	let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
	context.Map(Some(&dst_res), 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;

	let (w, h) = (desc.Width as usize, desc.Height as usize);
	let pitch = mapped.RowPitch as usize;
	let src_bytes = slice::from_raw_parts(mapped.pData as *const u8, h * pitch);
	let mut rgba = vec![0u8; w * h * 4];
	for y in 0..h {
		let row = &src_bytes[y * pitch..y * pitch + w * 4];
		let out = &mut rgba[y * w * 4..(y + 1) * w * 4];
		for x in 0..w {
			let p = x * 4;
			// Source is BGRA (DXGI B8G8R8A8); emit opaque RGBA.
			out[p] = row[p + 2];
			out[p + 1] = row[p + 1];
			out[p + 2] = row[p];
			out[p + 3] = 255;
		}
	}
	context.Unmap(Some(&dst_res), 0);

	let image = RgbaImage::from_raw(desc.Width, desc.Height, rgba).ok_or_else(|| anyhow!("rgba size mismatch"))?;
	Ok((desc.Width, desc.Height, image))
}
