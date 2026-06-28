//! PipeWire consumer for the ScreenCast portal's video stream.
//!
//! Linux-only (needs `libpipewire-0.3-dev` at build time). Runs the PipeWire
//! main loop on a dedicated thread, pulls frames from the portal's node,
//! converts them to RGBA, JPEG-encodes them, and forwards them to the session
//! writer.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use image::RgbaImage;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::Pod;
use tokio::sync::mpsc::Sender;

use crate::capture::encode_jpeg;
use crate::session::Encoded;

struct UserData {
	format: VideoInfoRaw,
	seq: u64,
	last_emit: Option<Instant>,
	min_interval: Duration,
	quality: u8,
	tx: Sender<Encoded>,
}

/// Spawn the capture thread. Returns immediately; the thread exits when `stop`
/// is set or the stream errors.
pub fn spawn(fd: OwnedFd, node_id: u32, quality: u8, max_fps: u8, stop: Arc<AtomicBool>, tx: Sender<Encoded>) {
	std::thread::spawn(move || {
		if let Err(e) = run(fd, node_id, quality, max_fps, stop, tx) {
			eprintln!("[libretether-agent] pipewire capture ended: {e}");
		}
	});
}

fn run(
	fd: OwnedFd,
	node_id: u32,
	quality: u8,
	max_fps: u8,
	stop: Arc<AtomicBool>,
	tx: Sender<Encoded>,
) -> Result<(), pw::Error> {
	pw::init();
	let mainloop = pw::main_loop::MainLoopRc::new(None)?;
	let context = pw::context::ContextRc::new(&mainloop, None)?;
	let core = context.connect_fd_rc(fd, None)?;

	let fps = max_fps.clamp(1, 60) as u64;
	let data = UserData {
		format: VideoInfoRaw::default(),
		seq: 0,
		last_emit: None,
		min_interval: Duration::from_millis(1000 / fps),
		quality,
		tx,
	};

	let stream = pw::stream::StreamBox::new(
		&core,
		"libretether-capture",
		properties! {
			*pw::keys::MEDIA_TYPE => "Video",
			*pw::keys::MEDIA_CATEGORY => "Capture",
			*pw::keys::MEDIA_ROLE => "Screen",
		},
	)?;

	let _listener = stream
		.add_local_listener_with_user_data(data)
		.param_changed(|_, user_data, id, param| {
			let Some(param) = param else { return };
			if id != spa::param::ParamType::Format.as_raw() {
				return;
			}
			let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param) else {
				return;
			};
			if media_type != spa::param::format::MediaType::Video
				|| media_subtype != spa::param::format::MediaSubtype::Raw
			{
				return;
			}
			let _ = user_data.format.parse(param);
		})
		.process(|stream, user_data| {
			let Some(mut buffer) = stream.dequeue_buffer() else {
				return;
			};
			let datas = buffer.datas_mut();
			if datas.is_empty() {
				return;
			}
			let data = &mut datas[0];
			let stride = data.chunk().stride().max(0) as usize;
			let (w, h) = (user_data.format.size().width, user_data.format.size().height);
			if w == 0 || h == 0 || stride == 0 {
				return;
			}
			// Frame-rate cap.
			if let Some(last) = user_data.last_emit {
				if last.elapsed() < user_data.min_interval {
					return;
				}
			}
			let format = user_data.format.format();
			let Some(bytes) = data.data() else { return };
			if let Some(rgba) = to_rgba(bytes, stride, w, h, format) {
				if let Ok(jpeg) = encode_jpeg(&rgba, user_data.quality) {
					user_data.seq += 1;
					let enc = Encoded {
						seq: user_data.seq,
						width: w,
						height: h,
						jpeg,
					};
					match user_data.tx.try_send(enc) {
						Ok(()) => user_data.last_emit = Some(Instant::now()),
						Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
							let _ = stream.disconnect();
						}
						Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {} // drop, stay realtime
					}
				}
			}
		})
		.register()?;

	// Offer the common packed 32-bit formats; the compositor picks one.
	let values = format_pod();
	let mut params = [Pod::from_bytes(&values).expect("valid format pod")];
	stream.connect(
		spa::utils::Direction::Input,
		Some(node_id),
		pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
		&mut params,
	)?;

	// Poll the stop flag and quit the loop when asked.
	let quit_loop = mainloop.clone();
	let timer = mainloop.loop_().add_timer(move |_| {
		if stop.load(Ordering::Relaxed) {
			quit_loop.quit();
		}
	});
	let _ = timer.update_timer(Some(Duration::from_millis(200)), Some(Duration::from_millis(200)));

	mainloop.run();
	Ok(())
}

/// Build the EnumFormat pod offering packed 32-bit RGB variants.
fn format_pod() -> Vec<u8> {
	use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
	use spa::param::video::VideoFormat as Vf;
	use spa::param::ParamType;
	use spa::pod::{serialize::PodSerializer, Value};
	use spa::utils::{Fraction, Rectangle};

	let obj = spa::pod::object!(
		spa::utils::SpaTypes::ObjectParamFormat,
		ParamType::EnumFormat,
		spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
		spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
		spa::pod::property!(
			FormatProperties::VideoFormat,
			Choice,
			Enum,
			Id,
			Vf::BGRx,
			Vf::BGRx,
			Vf::RGBx,
			Vf::BGRA,
			Vf::RGBA
		),
		spa::pod::property!(
			FormatProperties::VideoSize,
			Choice,
			Range,
			Rectangle,
			Rectangle {
				width: 1920,
				height: 1080
			},
			Rectangle { width: 1, height: 1 },
			Rectangle {
				width: 7680,
				height: 4320
			}
		),
		spa::pod::property!(
			FormatProperties::VideoFramerate,
			Choice,
			Range,
			Fraction,
			Fraction { num: 30, denom: 1 },
			Fraction { num: 0, denom: 1 },
			Fraction { num: 120, denom: 1 }
		),
	);
	PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
		.expect("serialize format pod")
		.0
		.into_inner()
}

/// Convert a packed 32-bit frame to RGBA, honoring the row stride.
fn to_rgba(src: &[u8], stride: usize, w: u32, h: u32, format: VideoFormat) -> Option<RgbaImage> {
	let (wi, hi) = (w as usize, h as usize);
	let mut out = vec![0u8; wi * hi * 4];
	// Byte order within each 4-byte source pixel.
	let (ri, gi, bi) = match format {
		VideoFormat::BGRx | VideoFormat::BGRA => (2, 1, 0),
		_ => (0, 1, 2), // RGBx / RGBA
	};
	for y in 0..hi {
		let row = src.get(y * stride..y * stride + wi * 4)?;
		for x in 0..wi {
			let p = x * 4;
			let o = (y * wi + x) * 4;
			out[o] = row[p + ri];
			out[o + 1] = row[p + gi];
			out[o + 2] = row[p + bi];
			out[o + 3] = 255;
		}
	}
	RgbaImage::from_raw(w, h, out)
}
