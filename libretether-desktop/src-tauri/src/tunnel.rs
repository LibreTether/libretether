//! Local TCP → agent tunneling, used to reach a client's RDP/SSH server through
//! the relay. We open a loopback listener and forward each connection over an
//! [`AgentLink`] stream; the agent connects to its own `127.0.0.1:remote_port`
//! and pipes. The launched RDP/SSH client just points at the local port.
//!
//! Listeners are registered on the [`ActiveController`] keyed by
//! `(client, remote_port)` and reused across reconnects, so repeated RDP/SSH
//! connects don't leak a fresh listener each time; they're torn down when the
//! client is removed or the controller exits.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use libretether_common::pipe_bidirectional;
use libretether_protocol::StreamOpen;
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::link::AgentLink;
use crate::state::{ActiveController, TunnelHandle};

/// How long a tunnel's accept loop keeps retrying through back-to-back errors
/// before giving up: a transient failure (a dropped connection mid-accept, or a
/// momentary fd/buffer exhaustion) clears within a few retries, but a listener
/// that fails this many times in a row is genuinely wedged — stop, mark the tunnel
/// dead, and let the next connect rebuild it.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: u32 = 64;
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Open (or reuse) a loopback listener forwarding to `remote_port` on the agent.
/// Returns the local port to point a client at.
pub async fn open(ctrl: &ActiveController, id: Uuid, link: AgentLink, remote_port: u16) -> AppResult<u16> {
	// Reuse a still-live tunnel for this (client, port) rather than leaking a new
	// listener on every connect. A tunnel whose accept loop has given up is dropped
	// here so we rebuild it instead of returning a dead local port.
	if let Some(port) = reuse_live(ctrl, (id, remote_port)) {
		crate::logbook::debug(
			"tunnel",
			&format!("reusing tunnel 127.0.0.1:{port} → {id} agent port {remote_port}"),
		);
		return Ok(port);
	}

	let listener = TcpListener::bind("127.0.0.1:0")
		.await
		.map_err(|e| AppError::msg(format!("binding tunnel: {e}")))?;
	let local_port = listener.local_addr().map_err(AppError::Io)?.port();
	crate::logbook::info(
		"tunnel",
		&format!("opened tunnel 127.0.0.1:{local_port} → {id} agent port {remote_port}"),
	);

	let alive = Arc::new(AtomicBool::new(true));
	let task = tauri::async_runtime::spawn(accept_loop(listener, link, remote_port, alive.clone()));

	// Register, but if a concurrent call already bound a live one for this key, keep
	// the existing one and drop ours so we don't leak the loser of the race. A stale
	// (dead) handle under this key is replaced and aborted.
	let mut tunnels = ctrl.tunnels.lock().unwrap();
	if let Some(h) = tunnels.get(&(id, remote_port)) {
		if h.alive.load(Ordering::Relaxed) {
			task.abort();
			return Ok(h.local_port);
		}
	}
	if let Some(stale) = tunnels.insert(
		(id, remote_port),
		TunnelHandle {
			local_port,
			task,
			alive,
		},
	) {
		stale.task.abort();
	}
	Ok(local_port)
}

/// Return the local port of a still-live tunnel for `key`, evicting (and aborting)
/// a dead one so the caller rebuilds it.
fn reuse_live(ctrl: &ActiveController, key: (Uuid, u16)) -> Option<u16> {
	let mut tunnels = ctrl.tunnels.lock().unwrap();
	match tunnels.get(&key) {
		Some(h) if h.alive.load(Ordering::Relaxed) => Some(h.local_port),
		Some(_) => {
			if let Some(dead) = tunnels.remove(&key) {
				dead.task.abort();
			}
			None
		}
		None => None,
	}
}

/// Accept loop for one tunnel listener. A per-connection `accept()` error is
/// transient (the listener fd stays valid), so we log, pause briefly to avoid a
/// busy-spin while the condition clears, and keep accepting — rather than tearing
/// down a tunnel the user still needs. Only a long run of consecutive failures
/// ends the loop, after which `alive` is cleared so a reuse rebuilds it.
async fn accept_loop(listener: TcpListener, link: AgentLink, remote_port: u16, alive: Arc<AtomicBool>) {
	let mut consecutive_errors = 0u32;
	loop {
		match listener.accept().await {
			Ok((tcp, _)) => {
				consecutive_errors = 0;
				crate::logbook::debug(
					"tunnel",
					&format!("accepted a local connection, forwarding to agent port {remote_port}"),
				);
				let link = link.clone();
				tauri::async_runtime::spawn(async move {
					if let Err(e) = forward(link, remote_port, tcp).await {
						crate::logbook::warn("tunnel", &format!("forward error: {e}"));
					}
				});
			}
			Err(e) => {
				consecutive_errors += 1;
				if consecutive_errors >= MAX_CONSECUTIVE_ACCEPT_ERRORS {
					crate::logbook::error("tunnel", &format!("accept gave up after repeated errors: {e}"));
					alive.store(false, Ordering::Relaxed);
					return;
				}
				crate::logbook::warn("tunnel", &format!("accept error (retrying): {e}"));
				tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
			}
		}
	}
}

/// Tear down any tunnels for a client (e.g. when it's removed).
pub fn close_for(ctrl: &ActiveController, id: Uuid) {
	let mut tunnels = ctrl.tunnels.lock().unwrap();
	let keys: Vec<_> = tunnels.keys().filter(|(cid, _)| *cid == id).copied().collect();
	if !keys.is_empty() {
		crate::logbook::debug("tunnel", &format!("closing {} tunnel(s) for {id}", keys.len()));
	}
	for key in keys {
		if let Some(h) = tunnels.remove(&key) {
			h.task.abort();
		}
	}
}

async fn forward(link: AgentLink, remote_port: u16, tcp: TcpStream) -> AppResult<()> {
	let (send, recv) = link
		.open_authenticated(StreamOpen::Tunnel { port: remote_port })
		.await?;

	// Pipe the local TCP connection to the agent stream in both directions; the
	// shared helper half-closes each side and waits for both (a shared select!
	// would truncate the tail of an SSH/RDP session).
	let (tcp_read, tcp_write) = tcp.into_split();
	pipe_bidirectional(tcp_read, tcp_write, recv, send).await;
	crate::logbook::debug(
		"tunnel",
		&format!("forwarded connection to agent port {remote_port} closed"),
	);
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::registry::ClientStore;
	use crate::state::{ControllerKind, ControllerProfile, TunnelHandle};

	fn controller() -> ActiveController {
		use std::sync::atomic::AtomicU32;
		static N: AtomicU32 = AtomicU32::new(0);
		let path = std::env::temp_dir().join(format!(
			"lt-tunnel-{}-{}.json",
			std::process::id(),
			N.fetch_add(1, Ordering::Relaxed)
		));
		let profile = ControllerProfile::new(
			"t".into(),
			ControllerKind::Direct {
				advertise_addr: None,
				listen_port: 0,
			},
		);
		let transfers = crate::transfer_queue::TransferQueue::load(path.with_extension("transfers")).unwrap();
		ActiveController::new(profile, ClientStore::load(path).unwrap(), transfers)
	}

	fn handle(alive: bool) -> TunnelHandle {
		TunnelHandle {
			local_port: 12345,
			task: tauri::async_runtime::spawn(async {}),
			alive: Arc::new(AtomicBool::new(alive)),
		}
	}

	// `reuse_live` is the guard that stops a tunnel whose accept loop has given up
	// from being handed back as a working local port — it must reuse a live tunnel
	// and evict (rebuild) a dead one.
	#[tokio::test]
	async fn reuse_live_keeps_a_live_tunnel_and_evicts_a_dead_one() {
		let ctrl = controller();
		let key = (Uuid::new_v4(), 3389);

		// A live tunnel is reused and stays registered.
		ctrl.tunnels.lock().unwrap().insert(key, handle(true));
		assert_eq!(reuse_live(&ctrl, key), Some(12345));
		assert!(ctrl.tunnels.lock().unwrap().contains_key(&key));

		// A dead tunnel is evicted so the caller rebuilds it.
		ctrl.tunnels.lock().unwrap().insert(key, handle(false));
		assert_eq!(reuse_live(&ctrl, key), None);
		assert!(!ctrl.tunnels.lock().unwrap().contains_key(&key));

		// A missing key is simply None.
		assert_eq!(reuse_live(&ctrl, (Uuid::new_v4(), 22)), None);
	}
}
