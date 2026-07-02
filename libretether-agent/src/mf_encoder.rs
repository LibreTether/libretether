//! Hardware H.264 on Windows via a Media Foundation encoder MFT.
//!
//! **Status: compiles clean, runtime-unvalidated.** Type-checked against `windows`
//! 0.59's Media Foundation bindings for the Windows target (clippy `-D warnings`),
//! and the Windows leg of the CI `rust` matrix rebuilds and tests it on
//! `windows-latest` on every push so it can't rot. It has **not** yet been run
//! against a real hardware encoder. It's compiled
//! into **every** Windows agent but **off by default at runtime**: the encoder is
//! only selected when `LIBRETETHER_ENCODER=hardware` is set (see
//! [`crate::encode::build_encoder`] / `DEFAULT_ENCODER_PREF`), so this untested
//! `unsafe` COM path can't run on a production guest by accident. If
//! [`MediaFoundationEncoder::new`] returns `Err` the encoder falls back to software
//! OpenH264, so even an explicit opt-in degrades safely rather than breaking the
//! session. Once it's confirmed on real GPUs, flipping the one `DEFAULT_ENCODER_PREF`
//! constant turns it on by default.
//!
//! To **validate on a Windows guest**, run any agent built for Windows with
//! `LIBRETETHER_ENCODER=hardware` and A/B it against `=software`; no special build or
//! feature flag is needed. See `.github/DEVELOPMENT.md` for the full recipe and what
//! to observe. The parts to scrutinise in that pass: the async-MFT event loop (this
//! drives the transform by *polling* its event queue and feeding one sample per call
//! rather than the strict wait-for-`METransformNeedInput` model — the first thing to
//! confirm on real hardware), in-band SPS/PPS insertion, `ICodecAPI` rate-control
//! values, and whether the chosen MFT wants D3D-backed (vs system-memory) input.
//!
//! ## Scope
//! This consumes a **system-memory NV12 frame** (converted from the captured RGBA)
//! and hardware-encodes it — i.e. it moves the H.264 *encode* off the CPU, the
//! dominant Windows cost. It does **not** yet eliminate the DXGI→CPU readback in
//! [`crate::wincap`]: true zero-copy needs a D3D11-backed input sample fed straight
//! from the duplication texture (an `MF_SA_D3D11_AWARE` MFT + a DXGI device
//! manager), which is a further step layered on this one.
//!
//! ## Pipeline shape
//! Hardware encoder MFTs are asynchronous, so this drives the transform through its
//! `IMFMediaEventGenerator` (`METransformNeedInput` / `METransformHaveOutput`) with
//! low-latency mode on, draining output for each input. The [`ScreenEncoder`] trait
//! is synchronous per frame; the decoder ignores frame ordering beyond delivery
//! order and the key/delta flag, so returning an output that lags its input by a
//! frame is harmless as long as bytes are delivered in order (which they are).

use anyhow::{anyhow, bail, Context, Result};
use std::sync::Once;

use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

use crate::encode::{rgba_to_nv12, starts_with_sps, ScreenEncoder};

/// Process-wide MF/COM init. MFStartup is refcounted, but doing it once for the
/// agent's lifetime is simplest and matches how the capture thread lives.
static MF_INIT: Once = Once::new();

fn ensure_mf_started() -> Result<()> {
	let mut result = Ok(());
	MF_INIT.call_once(|| {
		unsafe {
			// The agent's capture/encoder threads are their own OS threads; init COM
			// multithreaded (ignore RPC_E_CHANGED_MODE if something already did STA).
			let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
			if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
				result = Err(anyhow!("MFStartup failed: {e}"));
			}
		}
	});
	result
}

/// A configured Media Foundation H.264 encoder transform plus its input scratch.
pub struct MediaFoundationEncoder {
	transform: IMFTransform,
	events: IMFMediaEventGenerator,
	codec_api: ICodecAPI,
	/// SPS/PPS bytes (Annex-B) to prepend to each keyframe so the WebCodecs decoder
	/// can configure from in-band parameter sets. MF signals these out-of-band in
	/// `MF_MT_MPEG_SEQUENCE_HEADER`; we re-inject them on every IDR.
	sequence_header: Vec<u8>,
	nv12: Vec<u8>,
	width: usize,
	height: usize,
	/// Monotonic sample time in 100-ns units (MF wants timestamps; only monotonicity matters).
	frame_index: i64,
	frame_dur_hns: i64,
}

impl MediaFoundationEncoder {
	pub fn new(width: usize, height: usize, bitrate_kbps: u32, fps: u8) -> Result<Self> {
		ensure_mf_started()?;
		let fps = fps.max(1) as u32;
		unsafe {
			let transform = create_h264_encoder()?;

			// Async MFTs must be unlocked before use.
			let attrs = transform.GetAttributes().ok();
			if let Some(attrs) = attrs.as_ref() {
				let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
			}

			// Output type first (required order for encoders), then the matching input.
			let out_type = build_output_type(width as u32, height as u32, bitrate_kbps, fps)?;
			transform
				.SetOutputType(0, &out_type, 0)
				.context("SetOutputType(H264)")?;
			let in_type = build_input_type(width as u32, height as u32, fps)?;
			transform.SetInputType(0, &in_type, 0).context("SetInputType(NV12)")?;

			// Low-latency + CBR-ish rate control via the codec API.
			let codec_api: ICodecAPI = transform.cast().context("MFT has no ICodecAPI")?;
			let _ = codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
			let _ = codec_api.SetValue(
				&CODECAPI_AVEncCommonRateControlMode,
				&VARIANT::from(eAVEncCommonRateControlMode_CBR.0),
			);
			let _ = codec_api.SetValue(
				&CODECAPI_AVEncCommonMeanBitRate,
				&VARIANT::from(bitrate_kbps.saturating_mul(1000)),
			);

			// Grab the out-of-band SPS/PPS to re-inject on keyframes.
			let sequence_header = read_sequence_header(&transform).unwrap_or_default();

			// Begin streaming.
			transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0).ok();
			transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
			transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

			let events: IMFMediaEventGenerator = transform.cast().context("MFT is not an event generator")?;

			Ok(Self {
				transform,
				events,
				codec_api,
				sequence_header,
				nv12: vec![0u8; width * height + 2 * (width / 2) * (height / 2)],
				width,
				height,
				frame_index: 0,
				frame_dur_hns: 10_000_000 / fps as i64,
			})
		}
	}

	/// Wrap the current NV12 scratch in an `IMFSample` at the next timestamp.
	unsafe fn make_input_sample(&self) -> Result<IMFSample> {
		let len = self.nv12.len() as u32;
		let sample = MFCreateSample()?;
		let buffer = MFCreateMemoryBuffer(len)?;
		let mut ptr = std::ptr::null_mut::<u8>();
		buffer.Lock(&mut ptr, None, None)?;
		std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), ptr, self.nv12.len());
		buffer.Unlock()?;
		buffer.SetCurrentLength(len)?;
		sample.AddBuffer(&buffer)?;
		sample.SetSampleTime(self.frame_index * self.frame_dur_hns)?;
		sample.SetSampleDuration(self.frame_dur_hns)?;
		Ok(sample)
	}

	/// Pull all currently-available encoded outputs, appending `(is_key, annexb)`.
	unsafe fn drain_outputs(&mut self, out: &mut Vec<(bool, Vec<u8>)>) -> Result<()> {
		loop {
			// Wait for the next event; encoders emit NeedInput/HaveOutput.
			let event = match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
				Ok(ev) => ev,
				// No more events queued right now.
				Err(_) => return Ok(()),
			};
			let met = event.GetType()? as i32;
			if met == METransformHaveOutput.0 {
				if let Some(sample) = self.process_output()? {
					out.push(sample);
				}
			} else if met == METransformNeedInput.0 {
				// Nothing to feed here — `encode` feeds exactly one sample per call.
				return Ok(());
			}
		}
	}

	/// Pull one encoded sample, converting to `(is_key, annexb)`. Handles the async
	/// MFT's start-of-stream format change: a hardware encoder commonly answers the
	/// first `ProcessOutput` with `MF_E_TRANSFORM_STREAM_CHANGE` to hand over the real
	/// output type (and its SPS/PPS). We adopt the new type and retry rather than
	/// surfacing it as a spurious "encode failed".
	unsafe fn process_output(&mut self) -> Result<Option<(bool, Vec<u8>)>> {
		// Bound the retries so a misbehaving MFT that keeps renegotiating can't spin
		// this frame forever.
		for _ in 0..8 {
			let stream_info = self.transform.GetOutputStreamInfo(0)?;

			// Allocate the output sample unless the MFT provides its own.
			let provides = stream_info.dwFlags
				& (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32
				!= 0;
			let sample = if provides {
				None
			} else {
				let s = MFCreateSample()?;
				let b = MFCreateMemoryBuffer(stream_info.cbSize.max(1))?;
				s.AddBuffer(&b)?;
				Some(s)
			};

			let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
				dwStreamID: 0,
				pSample: std::mem::ManuallyDrop::new(sample.clone()),
				dwStatus: 0,
				pEvents: std::mem::ManuallyDrop::new(None),
			}];
			let mut status = 0u32;
			let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);

			// Reclaim the buffer handles we lent, whatever the outcome, so no COM
			// reference leaks on the need-more-input / stream-change / error paths.
			let produced = std::mem::ManuallyDrop::into_inner(std::mem::replace(
				&mut buffers[0].pSample,
				std::mem::ManuallyDrop::new(None),
			));
			let _events = std::mem::ManuallyDrop::into_inner(std::mem::replace(
				&mut buffers[0].pEvents,
				std::mem::ManuallyDrop::new(None),
			));

			match result {
				Ok(()) => {}
				Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
				// The encoder switched its output type (async hardware MFTs defer the
				// real type — and its SPS/PPS — to a stream change at start): adopt it
				// and try again.
				Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
					self.renegotiate_output()?;
					continue;
				}
				Err(e) => return Err(e.into()),
			}

			let produced = produced.ok_or_else(|| anyhow!("ProcessOutput produced no sample"))?;
			let is_key = produced.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
			let mut bytes = sample_to_vec(&produced)?;

			// WebCodecs configures from in-band SPS/PPS; MF keeps them out of band, so
			// prepend the sequence header to every keyframe.
			if is_key && !self.sequence_header.is_empty() && !starts_with_sps(&bytes) {
				let mut with_hdr = Vec::with_capacity(self.sequence_header.len() + bytes.len());
				with_hdr.extend_from_slice(&self.sequence_header);
				with_hdr.append(&mut bytes);
				bytes = with_hdr;
			}
			return Ok(Some((is_key, bytes)));
		}
		bail!("H.264 encoder kept renegotiating its output type; skipping this frame")
	}

	/// Adopt the encoder's new output type after `MF_E_TRANSFORM_STREAM_CHANGE`, and
	/// refresh the out-of-band SPS/PPS. Async hardware MFTs commonly defer the real
	/// output type — and its parameter sets — to a stream change at the very start;
	/// without adopting it, `ProcessOutput` never yields a frame.
	unsafe fn renegotiate_output(&mut self) -> Result<()> {
		let new_type = self
			.transform
			.GetOutputAvailableType(0, 0)
			.context("GetOutputAvailableType after stream change")?;
		self.transform
			.SetOutputType(0, &new_type, 0)
			.context("SetOutputType after stream change")?;
		// The real SPS/PPS is often only available now; refresh it (keeping the prior
		// header when the new type carries none) so keyframes still deliver parameter
		// sets to the WebCodecs decoder.
		if let Ok(hdr) = read_sequence_header(&self.transform) {
			if !hdr.is_empty() {
				self.sequence_header = hdr;
			}
		}
		Ok(())
	}
}

impl ScreenEncoder for MediaFoundationEncoder {
	fn encode(&mut self, rgba: &[u8], w: usize, h: usize, force_key: bool) -> Result<Option<(bool, Vec<u8>)>> {
		debug_assert_eq!((w, h), (self.width, self.height));
		unsafe {
			if force_key {
				let _ = self
					.codec_api
					.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
			}
			rgba_to_nv12(rgba, w, h, &mut self.nv12);
			let sample = self.make_input_sample()?;
			self.frame_index += 1;

			// Feed one input, then collect whatever the encoder has ready. In
			// low-latency mode this is typically this frame's output.
			match self.transform.ProcessInput(0, &sample, 0) {
				Ok(()) => {}
				Err(e) if e.code() == MF_E_NOTACCEPTING => {
					// Encoder is backed up; drain and drop this frame (stay real-time).
				}
				Err(e) => bail!("ProcessInput failed: {e}"),
			}

			let mut outputs = Vec::new();
			self.drain_outputs(&mut outputs)?;

			// Collapse to a single access unit for this call. If the encoder emitted
			// more than one, concatenate in order (still a valid Annex-B stream); if
			// none, this frame was buffered — report nothing (like the static gate).
			match outputs.len() {
				0 => Ok(None),
				1 => Ok(Some(outputs.pop().unwrap())),
				_ => {
					let is_key = outputs.iter().any(|(k, _)| *k);
					let mut merged = Vec::new();
					for (_, mut b) in outputs {
						merged.append(&mut b);
					}
					Ok(Some((is_key, merged)))
				}
			}
		}
	}

	fn kind(&self) -> &'static str {
		"Media Foundation (hardware)"
	}
}

impl Drop for MediaFoundationEncoder {
	fn drop(&mut self) {
		unsafe {
			let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
			let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
		}
	}
}

/// Enumerate video H.264 encoders, preferring a hardware MFT, and activate the
/// first one. Falls back to any (software) encoder if no hardware MFT is present —
/// though in that case the caller would do just as well with OpenH264.
unsafe fn create_h264_encoder() -> Result<IMFTransform> {
	let output_info = MFT_REGISTER_TYPE_INFO {
		guidMajorType: MFMediaType_Video,
		guidSubtype: MFVideoFormat_H264,
	};
	for flags in [
		MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
		MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
	] {
		let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
		let mut count = 0u32;
		MFTEnumEx(
			MFT_CATEGORY_VIDEO_ENCODER,
			flags,
			None,
			Some(&output_info),
			&mut activates,
			&mut count,
		)?;
		if count == 0 || activates.is_null() {
			continue;
		}
		let slice = std::slice::from_raw_parts(activates, count as usize);
		let mut chosen: Option<IMFTransform> = None;
		for activate in slice.iter().flatten() {
			if let Ok(transform) = activate.ActivateObject::<IMFTransform>() {
				chosen = Some(transform);
				break;
			}
		}
		windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
		if let Some(t) = chosen {
			return Ok(t);
		}
	}
	bail!("no usable H.264 encoder MFT found")
}

/// Build the H.264 output media type (subtype, bitrate, frame size, rate, profile).
unsafe fn build_output_type(w: u32, h: u32, bitrate_kbps: u32, fps: u32) -> Result<IMFMediaType> {
	let t = MFCreateMediaType()?;
	t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
	t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
	t.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps.saturating_mul(1000))?;
	t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
	// Baseline profile for the widest WebCodecs decode support.
	t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
	set_frame_size(&t, w, h)?;
	set_ratio(&t, &MF_MT_FRAME_RATE, fps, 1)?;
	set_ratio(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
	Ok(t)
}

/// Build the NV12 input media type matching the output geometry.
unsafe fn build_input_type(w: u32, h: u32, fps: u32) -> Result<IMFMediaType> {
	let t = MFCreateMediaType()?;
	t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
	t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
	t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
	set_frame_size(&t, w, h)?;
	set_ratio(&t, &MF_MT_FRAME_RATE, fps, 1)?;
	set_ratio(&t, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
	Ok(t)
}

/// Read the encoder's out-of-band SPS/PPS (`MF_MT_MPEG_SEQUENCE_HEADER`), if any.
unsafe fn read_sequence_header(transform: &IMFTransform) -> Result<Vec<u8>> {
	let out_type = transform.GetOutputCurrentType(0)?;
	let size = out_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER)?;
	if size == 0 {
		return Ok(Vec::new());
	}
	let mut buf = vec![0u8; size as usize];
	out_type.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None)?;
	Ok(buf)
}

/// `MF_MT_FRAME_SIZE` packs width/height into the high/low halves of a u64 attribute.
unsafe fn set_frame_size(t: &IMFMediaType, w: u32, h: u32) -> Result<()> {
	t.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
	Ok(())
}

/// A ratio attribute (numerator:denominator) is likewise packed into a u64.
unsafe fn set_ratio(t: &IMFMediaType, key: &GUID, num: u32, den: u32) -> Result<()> {
	t.SetUINT64(key, ((num as u64) << 32) | den as u64)?;
	Ok(())
}

/// Copy an `IMFSample`'s single contiguous buffer into a `Vec`.
unsafe fn sample_to_vec(sample: &IMFSample) -> Result<Vec<u8>> {
	let buffer = sample.ConvertToContiguousBuffer()?;
	let mut ptr = std::ptr::null_mut::<u8>();
	let mut len = 0u32;
	buffer.Lock(&mut ptr, None, Some(&mut len))?;
	let out = std::slice::from_raw_parts(ptr, len as usize).to_vec();
	buffer.Unlock()?;
	Ok(out)
}
