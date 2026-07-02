//! Zero-copy Windows hardware capture + encode (the controller's `Gpu` encoder).
//!
//! Keeps the frame on the GPU end to end. DXGI Desktop Duplication hands us a BGRA
//! D3D11 texture; a **video processor** converts (and downscales) it to NV12 on the
//! GPU; the NV12 texture is fed straight to a **Media Foundation H.264 encoder bound
//! to the same D3D11 device** via an MF DXGI device manager. There is no pixel
//! readback — only the compressed bitstream crosses to the CPU. This unifies the
//! capture + colour-convert + encode stages (normally three CPU passes across two
//! threads) into one GPU pipeline.
//!
//! It is entirely opt-in and self-contained: [`try_spawn`] attempts the setup and
//! returns `None` on any failure (older GPU, no video processor, encoder won't bind
//! to the device…), so the caller silently falls back to the standard CPU path. The
//! standard path is never touched.
//!
//! **Status: written but runtime-unvalidated** — like the CPU MF encoder was. It
//! compiles against the Windows API surface; the on-GPU behaviour (view/format
//! negotiation, the async encoder's texture lifetime) needs validation on real
//! hardware. Until then it only runs when the controller selects the `Gpu` encoder.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, TRUE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
	D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D,
	ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
	ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
	D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEX2D_VPIV,
	D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
	D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
	D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
	D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
	IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
	DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Media::MediaFoundation::{
	eAVEncCommonRateControlMode_CBR, CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
	CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFDXGIDeviceManager,
	IMFMediaEventGenerator, IMFTransform, METransformHaveOutput, METransformNeedInput, MFCreateDXGIDeviceManager,
	MFCreateDXGISurfaceBuffer, MFCreateMemoryBuffer, MFCreateSample, MFSampleExtension_CleanPoint,
	MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
	MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER,
	MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
	MF_EVENT_FLAG_NO_WAIT, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
	MF_TRANSFORM_ASYNC_UNLOCK,
};
use windows::Win32::System::Variant::VARIANT;

use libretether_protocol::video;

use crate::encode::{starts_with_sps, OutFrame, SharedConfig};
use crate::mf_encoder::{
	build_input_type, build_output_type, create_h264_encoder, ensure_mf_started, read_sequence_header, sample_to_vec,
};

/// How many NV12 output textures to cycle through. The async encoder may hold an
/// input for a frame or two after `ProcessInput`, so we must not overwrite the
/// texture it's still reading — a small ring gives it room while we blt the next.
const NV12_POOL: usize = 4;

/// Whether the zero-copy GPU path can be set up on this machine (cached): a D3D11
/// device with video support, a video device, a BGRA→NV12-capable video processor,
/// and a Media Foundation H.264 encoder. Advertised to the controller as the `Gpu`
/// encoder capability. The real per-session setup still falls back if something fails.
pub(crate) fn available() -> bool {
	static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
	*OK.get_or_init(|| unsafe { probe().is_ok() })
}

/// Lightweight probe for [`available`]: create the device + video device + a video
/// processor enumerator (no duplication, no encoder streaming) and confirm an H.264
/// MFT exists.
unsafe fn probe() -> Result<()> {
	ensure_mf_started()?;
	let mut device: Option<ID3D11Device> = None;
	D3D11CreateDevice(
		None,
		D3D_DRIVER_TYPE_HARDWARE,
		HMODULE::default(),
		D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
		None,
		D3D11_SDK_VERSION,
		Some(&mut device),
		None,
		None,
	)?;
	let device = device.ok_or_else(|| anyhow!("no device"))?;
	let vdevice: ID3D11VideoDevice = device.cast()?;
	let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
		InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
		InputFrameRate: DXGI_RATIONAL {
			Numerator: 60,
			Denominator: 1,
		},
		InputWidth: 640,
		InputHeight: 480,
		OutputFrameRate: DXGI_RATIONAL {
			Numerator: 60,
			Denominator: 1,
		},
		OutputWidth: 640,
		OutputHeight: 480,
		Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
	};
	let _enumerator = vdevice.CreateVideoProcessorEnumerator(&content)?;
	create_h264_encoder()?;
	Ok(())
}

/// Try to bring up the zero-copy pipeline and, on success, spawn its capture+encode
/// thread (emitting encoded [`OutFrame`]s to `out_tx`). Returns `None` if any part of
/// the setup fails, so the caller runs the standard CPU pipeline instead.
pub fn try_spawn(
	display: u32,
	shared: Arc<SharedConfig>,
	stop: Arc<AtomicBool>,
	out_tx: tokio::sync::mpsc::Sender<OutFrame>,
) -> Option<JoinHandle<()>> {
	// COM/D3D handles aren't `Send`, so the whole pipeline is set up *and* run on this
	// one thread — nothing crosses a thread boundary. Setup success/failure is reported
	// back on `ready` so the caller can fall back to the CPU pipeline synchronously.
	let (ready, ready_rx) = std::sync::mpsc::channel::<bool>();
	let handle = std::thread::spawn(move || {
		let gpu = match unsafe { Gpu::setup(display, &shared) } {
			Ok(gpu) => {
				crate::net::log("h264 encoder: Media Foundation (hardware, GPU zero-copy)");
				shared.report_capture("DXGI");
				shared.report_encoder("Media Foundation (GPU zero-copy)");
				let _ = ready.send(true);
				gpu
			}
			Err(e) => {
				crate::net::log(&format!("GPU zero-copy path unavailable ({e:#}); using CPU pipeline"));
				let _ = ready.send(false);
				return;
			}
		};
		if let Err(e) = unsafe { gpu.run(&shared, &stop, &out_tx) } {
			crate::net::log(&format!("gpu capture+encode ended: {e:#}"));
		}
	});
	match ready_rx.recv() {
		Ok(true) => Some(handle),
		// Setup failed (or the thread died): join it and let the caller use the CPU path.
		_ => {
			let _ = handle.join();
			None
		}
	}
}

/// The whole GPU pipeline: one D3D11 device shared by the duplication, the video
/// processor (BGRA→NV12 + scale) and the Media Foundation encoder.
struct Gpu {
	device: ID3D11Device,
	vdevice: ID3D11VideoDevice,
	// The immediate context, as its video interface — kept alive (it owns the shared
	// context the encoder + processor run on) and used to drive the video processor.
	vcontext: ID3D11VideoContext,
	duplication: IDXGIOutputDuplication,
	origin_x: i32,
	origin_y: i32,
	// Media Foundation encoder, bound to `device` via `_manager` (kept alive for its
	// lifetime; the MFT holds a reference).
	transform: IMFTransform,
	events: IMFMediaEventGenerator,
	codec_api: ICodecAPI,
	_manager: IMFDXGIDeviceManager,
	// Video processor for BGRA→NV12; rebuilt when the coded size changes.
	proc: Option<VideoProc>,
	// SPS/PPS to re-inject on keyframes (out of band from MF).
	sequence_header: Vec<u8>,
	// Coded output dimensions the encoder + processor are currently built for.
	built: (u32, u32, u32, u8), // (out_w, out_h, bitrate, fps)
	frame_index: i64,
	frame_dur_hns: i64,
}

/// The video-processor half, plus the NV12 output pool it blits into.
struct VideoProc {
	enumerator: ID3D11VideoProcessorEnumerator,
	processor: ID3D11VideoProcessor,
	/// NV12 textures + their output views, cycled for async-encoder safety.
	pool: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
	next: usize,
	in_w: u32,
	in_h: u32,
	out_w: u32,
	out_h: u32,
}

impl Gpu {
	unsafe fn setup(display: u32, shared: &SharedConfig) -> Result<Self> {
		ensure_mf_started()?;

		// One device for everything. VIDEO_SUPPORT unlocks the video processor; BGRA
		// for the duplicated desktop format. Not single-threaded — MF drives it from
		// its own worker, so enable multithread protection below.
		let mut device: Option<ID3D11Device> = None;
		let mut context: Option<ID3D11DeviceContext> = None;
		D3D11CreateDevice(
			None,
			D3D_DRIVER_TYPE_HARDWARE,
			HMODULE::default(),
			D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
			None,
			D3D11_SDK_VERSION,
			Some(&mut device),
			None,
			Some(&mut context),
		)?;
		let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?;
		let context = context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?;
		// MF shares this device across threads, so it must be multithread-protected.
		let mt: ID3D11Multithread = context.cast()?;
		let _ = mt.SetMultithreadProtected(true);

		let vdevice: ID3D11VideoDevice = device.cast().context("device has no video support")?;
		let vcontext: ID3D11VideoContext = context.cast()?;

		// Desktop duplication on the shared device.
		let dxgi_device: IDXGIDevice = device.cast()?;
		let adapter = dxgi_device.GetAdapter()?;
		let output = adapter.EnumOutputs(display)?;
		let output_desc = output.GetDesc()?;
		let output1: IDXGIOutput1 = output.cast()?;
		let duplication = output1.DuplicateOutput(&device)?;

		// MF DXGI device manager binding our device.
		let mut token = 0u32;
		let mut manager: Option<IMFDXGIDeviceManager> = None;
		MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
		let manager = manager.ok_or_else(|| anyhow!("MFCreateDXGIDeviceManager returned none"))?;
		manager.ResetDevice(&device, token)?;

		let transform = create_h264_encoder()?;
		if let Ok(attrs) = transform.GetAttributes() {
			let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
		}
		// Tell the MFT to use our D3D device (GPU input). The ULONG_PTR is the manager
		// pointer; `manager` is kept alive in `Self` for the MFT's lifetime.
		transform
			.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
			.context("MFT rejected the D3D manager")?;
		let events: IMFMediaEventGenerator = transform.cast().context("MFT is not an event generator")?;
		let codec_api: ICodecAPI = transform.cast().context("MFT has no ICodecAPI")?;

		let mut gpu = Self {
			device,
			vdevice,
			vcontext,
			duplication,
			origin_x: output_desc.DesktopCoordinates.left,
			origin_y: output_desc.DesktopCoordinates.top,
			transform,
			events,
			codec_api,
			_manager: manager,
			proc: None,
			sequence_header: Vec::new(),
			built: (0, 0, 0, 0),
			frame_index: 0,
			frame_dur_hns: 10_000_000 / 30,
		};
		// Build the encoder + processor for the first frame's geometry so setup fails
		// here (→ clean fallback) rather than mid-stream.
		let in_w = (output_desc.DesktopCoordinates.right - output_desc.DesktopCoordinates.left) as u32;
		let in_h = (output_desc.DesktopCoordinates.bottom - output_desc.DesktopCoordinates.top) as u32;
		let (ow, oh) = scaled_even(in_w, in_h, shared_scale(shared));
		gpu.rebuild(in_w, in_h, ow, oh, shared.bitrate_kbps(), shared.max_fps() as u8)?;
		Ok(gpu)
	}

	/// (Re)build the encoder + video processor for a new coded geometry / bitrate.
	unsafe fn rebuild(&mut self, in_w: u32, in_h: u32, out_w: u32, out_h: u32, bitrate: u32, fps: u8) -> Result<()> {
		let fps = fps.max(1);
		self.frame_dur_hns = 10_000_000 / fps as i64;

		// Encoder output/input types (H.264 out, NV12 in), then low-latency CBR config.
		let out_type = build_output_type(out_w, out_h, bitrate, fps as u32)?;
		self.transform.SetOutputType(0, &out_type, 0).context("SetOutputType")?;
		let in_type = build_input_type(out_w, out_h, fps as u32)?;
		self.transform.SetInputType(0, &in_type, 0).context("SetInputType")?;
		let _ = self
			.codec_api
			.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
		let _ = self.codec_api.SetValue(
			&CODECAPI_AVEncCommonRateControlMode,
			&VARIANT::from(eAVEncCommonRateControlMode_CBR.0),
		);
		let _ = self.codec_api.SetValue(
			&CODECAPI_AVEncCommonMeanBitRate,
			&VARIANT::from(bitrate.saturating_mul(1000)),
		);
		self.sequence_header = read_sequence_header(&self.transform).unwrap_or_default();

		self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0).ok();
		self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
		self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

		self.proc = Some(self.build_processor(in_w, in_h, out_w, out_h)?);
		self.built = (out_w, out_h, bitrate, fps);
		Ok(())
	}

	/// Create the video processor + NV12 output pool for `in`→`out` geometry.
	unsafe fn build_processor(&self, in_w: u32, in_h: u32, out_w: u32, out_h: u32) -> Result<VideoProc> {
		let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
			InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
			InputFrameRate: DXGI_RATIONAL {
				Numerator: 60,
				Denominator: 1,
			},
			InputWidth: in_w,
			InputHeight: in_h,
			OutputFrameRate: DXGI_RATIONAL {
				Numerator: 60,
				Denominator: 1,
			},
			OutputWidth: out_w,
			OutputHeight: out_h,
			Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
		};
		let enumerator = self.vdevice.CreateVideoProcessorEnumerator(&content)?;
		let processor = self.vdevice.CreateVideoProcessor(&enumerator, 0)?;

		// NV12 output textures + views, one per pool slot.
		let mut pool = Vec::with_capacity(NV12_POOL);
		for _ in 0..NV12_POOL {
			let desc = D3D11_TEXTURE2D_DESC {
				Width: out_w,
				Height: out_h,
				MipLevels: 1,
				ArraySize: 1,
				Format: DXGI_FORMAT_NV12,
				SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
				Usage: D3D11_USAGE_DEFAULT,
				BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
				CPUAccessFlags: 0,
				MiscFlags: 0,
			};
			let mut tex: Option<ID3D11Texture2D> = None;
			self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
			let tex = tex.ok_or_else(|| anyhow!("NV12 CreateTexture2D returned none"))?;
			let ovd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
				ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
				Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
					Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
				},
			};
			let tex_res: ID3D11Resource = tex.cast()?;
			let mut view: Option<ID3D11VideoProcessorOutputView> = None;
			self.vdevice
				.CreateVideoProcessorOutputView(&tex_res, &enumerator, &ovd, Some(&mut view))?;
			pool.push((tex, view.ok_or_else(|| anyhow!("no output view"))?));
		}
		Ok(VideoProc {
			enumerator,
			processor,
			pool,
			next: 0,
			in_w,
			in_h,
			out_w,
			out_h,
		})
	}

	/// The capture+encode loop. Runs until `stop` or the writer hangs up.
	unsafe fn run(
		mut self,
		shared: &SharedConfig,
		stop: &AtomicBool,
		out_tx: &tokio::sync::mpsc::Sender<OutFrame>,
	) -> Result<()> {
		let mut last_emit: Option<Instant> = None;
		let mut seq = 0u64;
		loop {
			if stop.load(Ordering::Relaxed) {
				break;
			}
			let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
			let mut resource: Option<IDXGIResource> = None;
			match self.duplication.AcquireNextFrame(200, &mut info, &mut resource) {
				Ok(()) => {}
				Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
				Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
					// Duplication lost (mode change): drop it and stop; the caller's next
					// session rebuilds. (A full in-place rebuild could be added later.)
					let _ = self.duplication.ReleaseFrame();
					bail!("desktop duplication access lost");
				}
				Err(e) => return Err(e.into()),
			}

			let present = info.LastPresentTime != 0;
			let interval = Duration::from_millis(1000 / shared.max_fps());
			let due = last_emit.is_none_or(|t| t.elapsed() >= interval);
			if present && due {
				if let Some(resource) = resource.as_ref() {
					let started = Instant::now();
					match self.encode_present(resource, shared, &mut seq) {
						Ok(Some(out)) => {
							let _ = started;
							if out_tx.blocking_send(out).is_err() {
								let _ = self.duplication.ReleaseFrame();
								break;
							}
							last_emit = Some(Instant::now());
						}
						Ok(None) => {}
						Err(e) => crate::net::log(&format!("gpu encode: {e:#}")),
					}
				}
			}
			self.duplication.ReleaseFrame()?;
		}
		Ok(())
	}

	/// Convert + encode one duplicated frame, returning an [`OutFrame`] when the
	/// encoder produced output this call.
	unsafe fn encode_present(
		&mut self,
		resource: &IDXGIResource,
		shared: &SharedConfig,
		seq: &mut u64,
	) -> Result<Option<OutFrame>> {
		let bgra: ID3D11Texture2D = resource.cast()?;
		let mut src_desc = D3D11_TEXTURE2D_DESC::default();
		bgra.GetDesc(&mut src_desc);
		let (in_w, in_h) = (src_desc.Width, src_desc.Height);
		let (out_w, out_h) = scaled_even(in_w, in_h, shared_scale(shared));
		let bitrate = shared.bitrate_kbps();
		let fps = shared.max_fps() as u8;

		// Rebuild if geometry / rate changed, or the processor's input size differs.
		let proc_matches = self
			.proc
			.as_ref()
			.is_some_and(|p| p.in_w == in_w && p.in_h == in_h && p.out_w == out_w && p.out_h == out_h);
		if !proc_matches || self.built != (out_w, out_h, bitrate, fps) {
			self.rebuild(in_w, in_h, out_w, out_h, bitrate, fps)?;
		}
		let force_key = shared.take_force_key();
		if force_key {
			let _ = self
				.codec_api
				.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
		}

		// GPU convert BGRA→NV12 (+ scale) into the next pool texture.
		let nv12 = self.blt_to_nv12(&bgra)?;

		// Wrap the NV12 texture in an IMFSample (no copy) and feed the encoder.
		let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &nv12, 0, false)?;
		let sample = MFCreateSample()?;
		sample.AddBuffer(&buffer)?;
		sample.SetSampleTime(self.frame_index * self.frame_dur_hns)?;
		sample.SetSampleDuration(self.frame_dur_hns)?;
		self.frame_index += 1;

		let mut outputs = Vec::new();
		match self.transform.ProcessInput(0, &sample, 0) {
			Ok(()) => {}
			// Same drain-then-retry as the CPU path: an async MFT rejects input until
			// its ready output is drained.
			Err(e) if e.code() == MF_E_NOTACCEPTING => {
				self.drain(&mut outputs)?;
				let _ = self.transform.ProcessInput(0, &sample, 0);
			}
			Err(e) => return Err(anyhow::Error::from(e).context("ProcessInput")),
		}
		self.drain(&mut outputs)?;

		if outputs.is_empty() {
			return Ok(None);
		}
		let is_key = outputs.iter().any(|(k, _)| *k);
		let mut au = Vec::new();
		for (_, mut b) in outputs {
			au.append(&mut b);
		}
		*seq += 1;
		let body = video::frame_message(is_key, *seq, out_w, out_h, &au);
		Ok(Some(OutFrame {
			source_width: in_w,
			source_height: in_h,
			origin_x: self.origin_x,
			origin_y: self.origin_y,
			body,
		}))
	}

	/// Blit BGRA `src` into the next NV12 pool texture, returning that texture.
	unsafe fn blt_to_nv12(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
		let proc = self.proc.as_mut().ok_or_else(|| anyhow!("no video processor"))?;
		let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
			FourCC: 0,
			ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
			Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
				Texture2D: D3D11_TEX2D_VPIV {
					MipSlice: 0,
					ArraySlice: 0,
				},
			},
		};
		let src_res: ID3D11Resource = src.cast()?;
		let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
		self.vdevice
			.CreateVideoProcessorInputView(&src_res, &proc.enumerator, &ivd, Some(&mut input_view))?;
		let input_view = input_view.ok_or_else(|| anyhow!("no input view"))?;

		let slot = proc.next;
		proc.next = (proc.next + 1) % proc.pool.len();
		let (ref nv12_tex, ref output_view) = proc.pool[slot];

		let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
			Enable: TRUE,
			..Default::default()
		};
		stream.pInputSurface = ManuallyDrop::new(Some(input_view));
		let blt = self
			.vcontext
			.VideoProcessorBlt(&proc.processor, output_view, 0, std::slice::from_ref(&stream));
		// Reclaim the input-view reference we lent the stream struct (ManuallyDrop won't
		// drop it on its own), whatever the blt returned.
		let _ = ManuallyDrop::into_inner(stream.pInputSurface);
		blt?;
		Ok(nv12_tex.clone())
	}

	/// Drain currently-available encoded outputs onto `out` as `(is_key, annexb)`.
	unsafe fn drain(&self, out: &mut Vec<(bool, Vec<u8>)>) -> Result<()> {
		loop {
			let event = match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
				Ok(ev) => ev,
				Err(_) => return Ok(()),
			};
			let met = event.GetType()? as i32;
			if met == METransformHaveOutput.0 {
				if let Some(s) = self.process_output()? {
					out.push(s);
				}
			} else if met == METransformNeedInput.0 {
				return Ok(());
			}
		}
	}

	/// One `ProcessOutput`, converting the encoded sample to `(is_key, annexb)`.
	unsafe fn process_output(&self) -> Result<Option<(bool, Vec<u8>)>> {
		let info = self.transform.GetOutputStreamInfo(0)?;
		let provides =
			info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32 != 0;
		let sample = if provides {
			None
		} else {
			let s = MFCreateSample()?;
			let b = MFCreateMemoryBuffer(info.cbSize.max(1))?;
			s.AddBuffer(&b)?;
			Some(s)
		};
		let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
			dwStreamID: 0,
			pSample: ManuallyDrop::new(sample.clone()),
			dwStatus: 0,
			pEvents: ManuallyDrop::new(None),
		}];
		let mut status = 0u32;
		let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
		let produced = ManuallyDrop::into_inner(std::mem::replace(&mut buffers[0].pSample, ManuallyDrop::new(None)));
		let _events = ManuallyDrop::into_inner(std::mem::replace(&mut buffers[0].pEvents, ManuallyDrop::new(None)));
		match result {
			Ok(()) => {}
			Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
			Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => return Ok(None),
			Err(e) => return Err(anyhow::Error::from(e).context("ProcessOutput")),
		}
		let produced = produced.ok_or_else(|| anyhow!("ProcessOutput produced no sample"))?;
		let is_key = produced.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
		let mut bytes = sample_to_vec(&produced)?;
		if is_key && !self.sequence_header.is_empty() && !starts_with_sps(&bytes) {
			let mut with = Vec::with_capacity(self.sequence_header.len() + bytes.len());
			with.extend_from_slice(&self.sequence_header);
			with.append(&mut bytes);
			bytes = with;
		}
		Ok(Some((is_key, bytes)))
	}
}

impl Drop for Gpu {
	fn drop(&mut self) {
		unsafe {
			let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
			let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
		}
	}
}

/// Downscale `in_w`×`in_h` by `scale` percent to even dimensions (4:2:0 needs even).
fn scaled_even(in_w: u32, in_h: u32, scale: u8) -> (u32, u32) {
	let w = ((in_w as u64 * scale as u64) / 100).max(2) as u32 & !1;
	let h = ((in_h as u64 * scale as u64) / 100).max(2) as u32 & !1;
	(w, h)
}

fn shared_scale(shared: &SharedConfig) -> u8 {
	shared.scale().clamp(10, 100)
}
