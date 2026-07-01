//! Binary wire format for live-session video, multiplexed on the session stream
//! alongside JSON [`SessionServer`] control messages.
//!
//! Each agent→controller message is one length-delimited blob (big-endian `u32`
//! length prefix, same as [`crate::frame`]) whose first byte is a tag:
//!
//! * [`TAG_CONTROL`] — the remaining bytes are a JSON [`SessionServer`]
//!   (`Meta`/`Error`). These are rare and small.
//! * [`TAG_FRAME`] — the remaining bytes are one H.264 access unit:
//!
//! ```text
//! u8   kind        0 = keyframe (IDR, carries SPS/PPS), 1 = delta (P-frame)
//! u64  seq         monotonic frame counter
//! u32  width       coded frame width  (after downscale, always even)
//! u32  height      coded frame height (after downscale, always even)
//! u32  len         access-unit byte length
//! u8[len]          H.264 Annex-B access unit (start-code-delimited NALs)
//! ```
//!
//! The decoder feeds the access unit straight to a WebCodecs `VideoDecoder` as an
//! `EncodedVideoChunk` (`type` = key→`"key"`, delta→`"delta"`). A keyframe is
//! self-contained — OpenH264 prepends SPS+PPS to every IDR — so the decoder can
//! (re)configure from any keyframe without a side-channel `description`. This
//! format is the single source of truth; the TypeScript decoder in the desktop
//! app mirrors it byte for byte.

use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::frame::MAX_FRAME;
use crate::SessionServer;

/// First byte of a session message: a JSON control message follows.
pub const TAG_CONTROL: u8 = 0;
/// First byte of a session message: a binary video frame follows.
pub const TAG_FRAME: u8 = 1;

/// `kind` byte: an IDR access unit (SPS/PPS + intra-coded picture), decodable on
/// its own.
pub const KIND_KEY: u8 = 0;
/// `kind` byte: a P-frame, decodable only on top of the preceding frames.
pub const KIND_DELTA: u8 = 1;

/// Fixed header size of a [`TAG_FRAME`] body, up to (but excluding) the access
/// unit bytes: `kind(1) + seq(8) + width(4) + height(4) + len(4)`.
pub const FRAME_HEADER_LEN: usize = 1 + 8 + 4 + 4 + 4;

/// Build a complete [`TAG_FRAME`] message body (tag byte included) wrapping one
/// H.264 access unit. `key` selects keyframe (IDR) vs delta (P). The caller writes
/// the result with [`write_message`].
pub fn frame_message(key: bool, seq: u64, width: u32, height: u32, au: &[u8]) -> Vec<u8> {
	let mut buf = Vec::with_capacity(1 + FRAME_HEADER_LEN + au.len());
	buf.push(TAG_FRAME);
	buf.push(if key { KIND_KEY } else { KIND_DELTA });
	buf.extend_from_slice(&seq.to_be_bytes());
	buf.extend_from_slice(&width.to_be_bytes());
	buf.extend_from_slice(&height.to_be_bytes());
	buf.extend_from_slice(&(au.len() as u32).to_be_bytes());
	buf.extend_from_slice(au);
	buf
}

/// Serialize a [`SessionServer`] control message to a [`TAG_CONTROL`] body.
pub fn control_message(msg: &SessionServer) -> std::io::Result<Vec<u8>> {
	let json = serde_json::to_vec(msg).map_err(|e| invalid(e.to_string()))?;
	let mut buf = Vec::with_capacity(1 + json.len());
	buf.push(TAG_CONTROL);
	buf.extend_from_slice(&json);
	Ok(buf)
}

/// Write one length-delimited session message (a body produced by
/// [`frame_message`] or [`control_message`]).
pub async fn write_message<W: AsyncWrite + Unpin>(w: &mut W, body: &[u8]) -> std::io::Result<()> {
	if body.len() as u64 > MAX_FRAME as u64 {
		return Err(invalid(format!("session message too large: {} bytes", body.len())));
	}
	w.write_all(&(body.len() as u32).to_be_bytes()).await?;
	w.write_all(body).await?;
	w.flush().await?;
	Ok(())
}

/// Convenience: serialize and write a control message.
pub async fn write_control<W: AsyncWrite + Unpin>(w: &mut W, msg: &SessionServer) -> std::io::Result<()> {
	write_message(w, &control_message(msg)?).await
}

/// One message read from the agent→controller direction of a session stream.
pub enum Inbound {
	/// A JSON control message (`Meta`/`Error`).
	Control(SessionServer),
	/// A raw binary video-frame body, tag byte stripped — ready to forward to the
	/// webview decoder as-is (the controller never decodes pixels itself).
	Frame(Vec<u8>),
}

/// Read one session message and classify it. The frame body is returned without
/// the leading tag byte so it can be forwarded straight to the decoder.
pub async fn read_inbound<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Inbound> {
	let body = read_message(r).await?;
	match body.first() {
		Some(&TAG_CONTROL) => Ok(Inbound::Control(parse_json(&body[1..])?)),
		Some(&TAG_FRAME) => Ok(Inbound::Frame(body[1..].to_vec())),
		Some(other) => Err(invalid(format!("unknown session message tag {other}"))),
		None => Err(invalid("empty session message")),
	}
}

/// Read one length-delimited session message body, rejecting anything over [`MAX_FRAME`].
async fn read_message<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
	let mut len = [0u8; 4];
	r.read_exact(&mut len).await?;
	let n = u32::from_be_bytes(len);
	if n > MAX_FRAME {
		return Err(invalid(format!(
			"session message too large: {n} bytes (max {MAX_FRAME})"
		)));
	}
	let mut buf = vec![0u8; n as usize];
	r.read_exact(&mut buf).await?;
	Ok(buf)
}

fn parse_json<T: DeserializeOwned>(bytes: &[u8]) -> std::io::Result<T> {
	serde_json::from_slice(bytes).map_err(|e| invalid(e.to_string()))
}

fn invalid(msg: impl Into<String>) -> std::io::Error {
	std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn frame_message_round_trips_over_a_stream() {
		let au = vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f]; // a fake Annex-B SPS start
		let body = frame_message(true, 7, 640, 480, &au);
		let (mut a, mut b) = tokio::io::duplex(4096);
		write_message(&mut a, &body).await.unwrap();

		match read_inbound(&mut b).await.unwrap() {
			Inbound::Frame(payload) => {
				assert_eq!(payload[0], KIND_KEY);
				assert_eq!(u64::from_be_bytes(payload[1..9].try_into().unwrap()), 7);
				assert_eq!(u32::from_be_bytes(payload[9..13].try_into().unwrap()), 640);
				assert_eq!(u32::from_be_bytes(payload[13..17].try_into().unwrap()), 480);
				assert_eq!(u32::from_be_bytes(payload[17..21].try_into().unwrap()), au.len() as u32);
				assert_eq!(&payload[21..], &au[..]);
			}
			_ => panic!("expected a frame"),
		}
	}

	#[tokio::test]
	async fn delta_kind_is_tagged() {
		let body = frame_message(false, 2, 320, 240, &[9, 9, 9]);
		let (mut a, mut b) = tokio::io::duplex(4096);
		write_message(&mut a, &body).await.unwrap();
		match read_inbound(&mut b).await.unwrap() {
			Inbound::Frame(payload) => assert_eq!(payload[0], KIND_DELTA),
			_ => panic!("expected a frame"),
		}
	}

	#[tokio::test]
	async fn control_message_round_trips_as_json() {
		let (mut a, mut b) = tokio::io::duplex(4096);
		write_control(
			&mut a,
			&SessionServer::Meta {
				display: 1,
				width: 1920,
				height: 1080,
			},
		)
		.await
		.unwrap();
		match read_inbound(&mut b).await.unwrap() {
			Inbound::Control(SessionServer::Meta { display, width, height }) => {
				assert_eq!((display, width, height), (1, 1920, 1080));
			}
			_ => panic!("expected a control message"),
		}
	}
}
