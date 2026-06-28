//! Length-delimited JSON framing over any async byte stream (e.g. a QUIC
//! stream). Each message is a big-endian `u32` length prefix followed by that
//! many bytes of JSON.

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Hard cap on a single frame. Generous enough for full-screen PNG/JPEG frames
/// while still rejecting absurd lengths from a misbehaving peer.
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

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

/// Read one length-delimited frame and deserialize it from JSON.
pub async fn read_frame<R, T>(r: &mut R) -> std::io::Result<T>
where
	R: AsyncRead + Unpin,
	T: DeserializeOwned,
{
	let mut len = [0u8; 4];
	r.read_exact(&mut len).await?;
	let n = u32::from_be_bytes(len);
	if n > MAX_FRAME {
		return Err(invalid(format!("frame too large: {n} bytes")));
	}
	let mut buf = vec![0u8; n as usize];
	r.read_exact(&mut buf).await?;
	serde_json::from_slice(&buf).map_err(|e| invalid(e.to_string()))
}
