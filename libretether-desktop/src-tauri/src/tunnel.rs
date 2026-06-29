//! Local TCP → agent tunneling, used to reach a client's RDP/SSH server through
//! the relay. We open a loopback listener and forward each connection over an
//! [`AgentLink`] stream; the agent connects to its own `127.0.0.1:remote_port`
//! and pipes. The launched RDP/SSH client just points at the local port.
//!
//! Listeners are registered on the [`ActiveController`] keyed by
//! `(client, remote_port)` and reused across reconnects, so repeated RDP/SSH
//! connects don't leak a fresh listener each time; they're torn down when the
//! client is removed or the controller exits.

use libretether_common::pipe_bidirectional;
use libretether_protocol::frame::write_frame;
use libretether_protocol::StreamOpen;
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::link::AgentLink;
use crate::state::{ActiveController, TunnelHandle};

/// Open (or reuse) a loopback listener forwarding to `remote_port` on the agent.
/// Returns the local port to point a client at.
pub async fn open(ctrl: &ActiveController, id: Uuid, link: AgentLink, remote_port: u16) -> AppResult<u16> {
	// Reuse an existing tunnel for this (client, port) rather than leaking a new
	// listener on every connect.
	if let Some(h) = ctrl.tunnels.lock().unwrap().get(&(id, remote_port)) {
		return Ok(h.local_port);
	}

	let listener = TcpListener::bind("127.0.0.1:0")
		.await
		.map_err(|e| AppError::msg(format!("binding tunnel: {e}")))?;
	let local_port = listener.local_addr().map_err(AppError::Io)?.port();

	let task = tauri::async_runtime::spawn(async move {
		while let Ok((tcp, _)) = listener.accept().await {
			let link = link.clone();
			tauri::async_runtime::spawn(async move {
				if let Err(e) = forward(link, remote_port, tcp).await {
					eprintln!("[libretether] tunnel forward error: {e}");
				}
			});
		}
	});

	// Register, but if a concurrent call already bound one for this key, keep the
	// existing one and drop ours so we don't leak the loser of the race.
	let mut tunnels = ctrl.tunnels.lock().unwrap();
	if let Some(h) = tunnels.get(&(id, remote_port)) {
		task.abort();
		return Ok(h.local_port);
	}
	tunnels.insert((id, remote_port), TunnelHandle { local_port, task });
	Ok(local_port)
}

/// Tear down any tunnels for a client (e.g. when it's removed).
pub fn close_for(ctrl: &ActiveController, id: Uuid) {
	let mut tunnels = ctrl.tunnels.lock().unwrap();
	let keys: Vec<_> = tunnels.keys().filter(|(cid, _)| *cid == id).copied().collect();
	for key in keys {
		if let Some(h) = tunnels.remove(&key) {
			h.task.abort();
		}
	}
}

async fn forward(link: AgentLink, remote_port: u16, tcp: TcpStream) -> AppResult<()> {
	let (mut send, recv) = link.open_bi().await?;
	write_frame(&mut send, &StreamOpen::Tunnel { port: remote_port }).await?;
	link.authenticate(&mut send).await?;

	// Pipe the local TCP connection to the agent stream in both directions; the
	// shared helper half-closes each side and waits for both (a shared select!
	// would truncate the tail of an SSH/RDP session).
	let (tcp_read, tcp_write) = tcp.into_split();
	pipe_bidirectional(tcp_read, tcp_write, recv, send).await;
	Ok(())
}
