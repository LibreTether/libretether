//! Length-delimited JSON framing over any async byte stream (e.g. a QUIC
//! stream). Each message is a big-endian `u32` length prefix followed by that
//! many bytes of JSON.

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Hard cap on a single frame. Generous enough for full-screen PNG/JPEG frames
/// while still rejecting absurd lengths from a misbehaving peer.
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

/// Tight cap for small control/handshake/relay frames — everything except live
/// session frames and screenshots. Keeps a peer (in particular one that has not
/// authenticated yet) from forcing a large up-front allocation.
pub const MAX_CONTROL_FRAME: u32 = 1024 * 1024;

fn invalid(msg: impl Into<String>) -> std::io::Error {
	std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into())
}

/// Serialize `msg` as JSON and write it as one length-delimited frame.
pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
	W: AsyncWrite + Unpin,
	T: Serialize + ?Sized,
{
	let bytes = serde_json::to_vec(msg).map_err(|e| invalid(e.to_string()))?;
	if bytes.len() as u64 > MAX_FRAME as u64 {
		return Err(invalid(format!("frame too large: {} bytes", bytes.len())));
	}
	w.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
	w.write_all(&bytes).await?;
	w.flush().await?;
	Ok(())
}

/// Read one length-delimited frame and deserialize it from JSON, rejecting any
/// frame larger than [`MAX_FRAME`].
pub async fn read_frame<R, T>(r: &mut R) -> std::io::Result<T>
where
	R: AsyncRead + Unpin,
	T: DeserializeOwned,
{
	read_frame_capped(r, MAX_FRAME).await
}

/// Like [`read_frame`] but rejects any frame larger than `max`. Use a tight
/// `max` (e.g. [`MAX_CONTROL_FRAME`]) for small control/handshake messages so a
/// peer can't force a large allocation with a bogus length prefix.
pub async fn read_frame_capped<R, T>(r: &mut R, max: u32) -> std::io::Result<T>
where
	R: AsyncRead + Unpin,
	T: DeserializeOwned,
{
	let mut len = [0u8; 4];
	r.read_exact(&mut len).await?;
	let n = u32::from_be_bytes(len);
	if n > max {
		return Err(invalid(format!("frame too large: {n} bytes (max {max})")));
	}
	let mut buf = vec![0u8; n as usize];
	r.read_exact(&mut buf).await?;
	serde_json::from_slice(&buf).map_err(|e| invalid(e.to_string()))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn round_trips_a_value() {
		let (mut a, mut b) = tokio::io::duplex(4096);
		let msg = vec!["one".to_string(), "two".to_string()];
		write_frame(&mut a, &msg).await.unwrap();
		let got: Vec<String> = read_frame(&mut b).await.unwrap();
		assert_eq!(got, msg);
	}

	#[tokio::test]
	async fn write_rejects_oversize_frames() {
		// An absurd payload over the global cap is refused on write.
		let huge = "y".repeat(MAX_FRAME as usize + 1);
		assert!(write_frame(&mut Vec::new(), &huge).await.is_err());
		// A control-sized payload still writes fine under the global cap.
		let ok = "x".repeat(MAX_CONTROL_FRAME as usize + 1);
		assert!(write_frame(&mut Vec::new(), &ok).await.is_ok());
	}

	#[tokio::test]
	async fn read_capped_rejects_a_frame_over_the_cap() {
		// Write a frame larger than MAX_CONTROL_FRAME, then read it under the tight
		// cap — the reader must reject it on the length prefix alone.
		let (mut a, mut b) = tokio::io::duplex(1024 * 1024 * 2);
		let payload = "z".repeat(MAX_CONTROL_FRAME as usize + 10);
		write_frame(&mut a, &payload).await.unwrap();
		let res: std::io::Result<String> = read_frame_capped(&mut b, MAX_CONTROL_FRAME).await;
		assert!(res.is_err());
	}
}
