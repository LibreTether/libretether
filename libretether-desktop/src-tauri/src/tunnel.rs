//! Local TCP → agent tunneling, used to reach a client's RDP/SSH server through
//! the relay. We open a loopback listener and forward each connection over an
//! [`AgentLink`] stream; the agent connects to its own `127.0.0.1:remote_port`
//! and pipes. The launched RDP/SSH client just points at the local port.

use libretether_protocol::frame::write_frame;
use libretether_protocol::StreamOpen;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use crate::error::{AppError, AppResult};
use crate::link::AgentLink;

/// Open a loopback listener that forwards to `remote_port` on the agent. Returns
/// the local port to point a client at. The listener runs until the app exits.
pub async fn open(link: AgentLink, remote_port: u16) -> AppResult<u16> {
	let listener = TcpListener::bind("127.0.0.1:0")
		.await
		.map_err(|e| AppError::msg(format!("binding tunnel: {e}")))?;
	let port = listener.local_addr().map_err(AppError::Io)?.port();

	tauri::async_runtime::spawn(async move {
		while let Ok((tcp, _)) = listener.accept().await {
			let link = link.clone();
			tauri::async_runtime::spawn(async move {
				if let Err(e) = forward(link, remote_port, tcp).await {
					eprintln!("[libretether] tunnel forward error: {e}");
				}
			});
		}
	});
	Ok(port)
}

async fn forward(link: AgentLink, remote_port: u16, tcp: TcpStream) -> AppResult<()> {
	let (mut send, mut recv) = link.open_bi().await?;
	write_frame(&mut send, &StreamOpen::Tunnel { port: remote_port }).await?;

	let (mut tcp_read, mut tcp_write) = tcp.into_split();
	// Half-close each direction when its source ends, then wait for BOTH — a
	// shared select! would tear the peer direction down on first EOF and truncate
	// the stream (e.g. dropping the tail of an SSH/RDP session).
	let up = async {
		let _ = tokio::io::copy(&mut tcp_read, &mut send).await;
		let _ = send.finish();
	};
	let down = async {
		let _ = tokio::io::copy(&mut recv, &mut tcp_write).await;
		let _ = tcp_write.shutdown().await;
	};
	tokio::join!(up, down);
	Ok(())
}
