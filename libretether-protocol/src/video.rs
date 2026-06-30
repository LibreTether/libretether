//! Binary wire format for live-session video, multiplexed on the session stream
//! alongside JSON [`SessionServer`] control messages.
//!
//! The previous format JSON-wrapped a base64 string per frame — ~33% bandwidth
//! overhead plus a full re-serialize on every hop. This replaces it with a
//! compact binary frame carrying raw JPEG bytes for only the screen *tiles* that
//! changed since the last frame (delta encoding), with periodic full keyframes.
//!
//! Each message on the agent→controller direction is one length-delimited blob
//! (big-endian `u32` length prefix, same as [`crate::frame`]) whose first byte is
//! a tag:
//!
//! * [`TAG_CONTROL`] — the remaining bytes are a JSON [`SessionServer`]
//!   (`Meta`/`Error`). These are rare and small.
//! * [`TAG_FRAME`] — the remaining bytes are a binary video frame:
//!
//! ```text
//! u8   kind        0 = keyframe (every tile), 1 = delta (changed tiles only)
//! u64  seq         monotonic frame counter
//! u32  width       encoded frame width  (after downscale)
//! u32  height      encoded frame height (after downscale)
//! u16  tile_size   grid cell size in px; a tile's pixel origin is col*tile_size, row*tile_size
//! u32  count       number of tiles in this message
//! count × {
//!   u16 col        tile column
//!   u16 row        tile row
//!   u32 jpeg_len
//!   u8[jpeg_len]   baseline JPEG of this tile
//! }
//! ```
//!
//! The decoder composites tiles onto a canvas at `(col*tile_size, row*tile_size)`;
//! a keyframe first resizes/clears the canvas to `width`×`height`. This format is
//! the single source of truth — the TypeScript decoder in the desktop app mirrors
//! it byte for byte.

use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::frame::MAX_FRAME;
use crate::SessionServer;

/// First byte of a session message: a JSON control message follows.
pub const TAG_CONTROL: u8 = 0;
/// First byte of a session message: a binary video frame follows.
pub const TAG_FRAME: u8 = 1;

/// `kind` byte: every tile is present (a full frame).
pub const KIND_KEY: u8 = 0;
/// `kind` byte: only changed tiles are present.
pub const KIND_DELTA: u8 = 1;

/// One encoded screen tile at grid position `(col, row)`.
pub struct Tile {
	pub col: u16,
	pub row: u16,
	pub jpeg: Vec<u8>,
}

/// Build a complete [`TAG_FRAME`] message body (tag byte included) for the given
/// tiles. `key` selects keyframe vs delta. The caller writes the result with
/// [`write_message`].
pub fn frame_message(key: bool, seq: u64, width: u32, height: u32, tile_size: u16, tiles: &[Tile]) -> Vec<u8> {
	let payload: usize = tiles.iter().map(|t| 2 + 2 + 4 + t.jpeg.len()).sum();
	let mut buf = Vec::with_capacity(1 + 1 + 8 + 4 + 4 + 2 + 4 + payload);
	buf.push(TAG_FRAME);
	buf.push(if key { KIND_KEY } else { KIND_DELTA });
	buf.extend_from_slice(&seq.to_be_bytes());
	buf.extend_from_slice(&width.to_be_bytes());
	buf.extend_from_slice(&height.to_be_bytes());
	buf.extend_from_slice(&tile_size.to_be_bytes());
	buf.extend_from_slice(&(tiles.len() as u32).to_be_bytes());
	for t in tiles {
		buf.extend_from_slice(&t.col.to_be_bytes());
		buf.extend_from_slice(&t.row.to_be_bytes());
		buf.extend_from_slice(&(t.jpeg.len() as u32).to_be_bytes());
		buf.extend_from_slice(&t.jpeg);
	}
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
		let tiles = vec![
			Tile {
				col: 0,
				row: 0,
				jpeg: vec![1, 2, 3],
			},
			Tile {
				col: 3,
				row: 1,
				jpeg: vec![9, 8],
			},
		];
		let body = frame_message(true, 7, 640, 480, 256, &tiles);
		let (mut a, mut b) = tokio::io::duplex(4096);
		write_message(&mut a, &body).await.unwrap();

		match read_inbound(&mut b).await.unwrap() {
			Inbound::Frame(payload) => {
				// kind=key, seq=7, then geometry — spot-check the header.
				assert_eq!(payload[0], KIND_KEY);
				assert_eq!(u64::from_be_bytes(payload[1..9].try_into().unwrap()), 7);
				assert_eq!(u32::from_be_bytes(payload[9..13].try_into().unwrap()), 640);
				assert_eq!(u32::from_be_bytes(payload[13..17].try_into().unwrap()), 480);
				assert_eq!(u16::from_be_bytes(payload[17..19].try_into().unwrap()), 256);
				assert_eq!(u32::from_be_bytes(payload[19..23].try_into().unwrap()), 2);
			}
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
