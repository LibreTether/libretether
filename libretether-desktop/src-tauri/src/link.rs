//! How the controller reaches an agent — directly (the agent dialed us), through
//! the relay, or over an **upgraded direct peer-to-peer path** punched through NAT.
//! Everything that talks to an agent goes through [`AgentLink::open_authenticated`],
//! so the control plane is identical in every mode.
//!
//! After the mutual handshake the agent issues a per-connection capability token
//! (see the protocol's `SessionGrant`) and both ends agree an end-to-end session
//! key (see [`libretether_protocol::e2e`]). The link carries both, and
//! [`AgentLink::open_authenticated`] wraps every non-handshake stream in the AEAD
//! record layer — sending the capability token *through* it — so a relay only ever
//! forwards ciphertext and the agent can still tell a verified controller's streams
//! apart from anything else the relay might route to it.
//!
//! ## Peer-to-peer upgrade
//!
//! A relay link can carry an optional [`DirectUpgrade`]: once a direct QUIC path to
//! the agent is punched through NAT and completes its own handshake, new streams
//! prefer it (lower latency, no relay egress). Every clone of the link shares one
//! upgrade slot, so an upgrade installed after a session started is picked up by that
//! session's *next* stream, and if the direct path drops, streams transparently fall
//! back to the relay. The direct connection has its own capability token and session
//! key (from its own handshake), so the wrapping is identical — only the transport
//! differs.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use libretether_protocol::e2e::{self, SecureQuicRecv, SecureQuicSend, SessionKey};
use libretether_protocol::frame::write_frame;
use libretether_protocol::relay::RelayRequest;
use libretether_protocol::{StreamAuth, StreamOpen};
use quinn::{Connection, RecvStream, SendStream};

use crate::error::{AppError, AppResult};

#[derive(Clone)]
enum Transport {
	/// The agent dialed us directly (Tailscale / Direct mode).
	Direct(Connection),
	/// Reach the agent through the relay, addressed by its public key.
	Relay { relay: Connection, agent: String },
}

/// An upgraded direct peer-to-peer path to the agent: the punched-through QUIC
/// connection plus the capability token and end-to-end key from *its* handshake.
/// Cheap to clone (a connection handle and two `Arc`s).
#[derive(Clone)]
pub struct DirectUpgrade {
	conn: Connection,
	token: Arc<str>,
	session_key: Arc<SessionKey>,
}

#[derive(Clone)]
pub struct AgentLink {
	transport: Transport,
	/// Capability token from the base transport's handshake; `None` until it completes
	/// (the handshake stream itself carries no token).
	token: Option<Arc<str>>,
	/// End-to-end session key from the base transport's handshake; `None` until it
	/// completes. Every non-handshake stream is sealed under a key derived from it.
	session_key: Option<Arc<SessionKey>>,
	/// A relay link's optional upgraded direct path (see [`DirectUpgrade`]). Shared
	/// across clones so an upgrade — or its loss — is seen by every holder of the link.
	/// `None` for a `Direct` link (there's no relay to upgrade off).
	upgrade: Option<Arc<Mutex<Option<DirectUpgrade>>>>,
}

impl AgentLink {
	pub fn direct(conn: Connection) -> Self {
		Self {
			transport: Transport::Direct(conn),
			token: None,
			session_key: None,
			upgrade: None,
		}
	}

	pub fn relay(relay: Connection, agent: String) -> Self {
		Self {
			transport: Transport::Relay { relay, agent },
			token: None,
			session_key: None,
			// A relay link can be upgraded to a direct P2P path later.
			upgrade: Some(Arc::new(Mutex::new(None))),
		}
	}

	/// Return a copy of this link that carries the handshake's capability token and
	/// end-to-end session key, so its streams are stamped and encrypted. The upgrade
	/// slot (if any) is shared, not reset — a `with_session` clone still sees any
	/// direct path installed on the original.
	pub fn with_session(&self, token: String, session_key: SessionKey) -> Self {
		Self {
			transport: self.transport.clone(),
			token: Some(Arc::from(token.as_str())),
			session_key: Some(Arc::new(session_key)),
			upgrade: self.upgrade.clone(),
		}
	}

	/// Install (or replace) the upgraded direct path for this relay link, with the
	/// capability token and session key from the direct connection's own handshake.
	/// A no-op on a non-relay link. Visible immediately to every clone of the link.
	pub fn set_upgrade(&self, conn: Connection, token: String, session_key: SessionKey) {
		if let Some(slot) = &self.upgrade {
			*slot.lock().unwrap() = Some(DirectUpgrade {
				conn,
				token: Arc::from(token.as_str()),
				session_key: Arc::new(session_key),
			});
		}
	}

	/// Drop any installed direct path, so subsequent streams fall back to the relay.
	pub fn clear_upgrade(&self) {
		if let Some(slot) = &self.upgrade {
			*slot.lock().unwrap() = None;
		}
	}

	/// Drop the installed direct path **only if** it is `conn` — used by the direct
	/// connection's close monitor so a newer punch that already replaced the upgrade
	/// isn't torn down. Returns whether it cleared anything.
	pub fn clear_upgrade_for(&self, conn: &Connection) -> bool {
		let Some(slot) = &self.upgrade else { return false };
		let mut guard = slot.lock().unwrap();
		if guard.as_ref().map(|u| u.conn.stable_id()) == Some(conn.stable_id()) {
			*guard = None;
			return true;
		}
		false
	}

	/// Whether a healthy direct P2P path is currently in use for new streams.
	pub fn is_upgraded(&self) -> bool {
		self.healthy_upgrade().is_some()
	}

	/// The current healthy upgrade, if any. A direct connection that has closed is
	/// cleared here and treated as absent, so the very next stream falls back to the
	/// relay without needing an external monitor.
	fn healthy_upgrade(&self) -> Option<DirectUpgrade> {
		let slot = self.upgrade.as_ref()?;
		let mut guard = slot.lock().unwrap();
		match guard.as_ref() {
			Some(up) if up.conn.close_reason().is_none() => Some(up.clone()),
			Some(_) => {
				*guard = None; // the direct path dropped — fall back to the relay
				None
			}
			None => None,
		}
	}

	/// Open a raw bidirectional stream on the **base** transport (relay or direct) — for
	/// the unauthenticated, unencrypted handshake. This never uses a direct upgrade: the
	/// handshake establishes trust on the very connection it runs over, and the P2P
	/// upgrade path runs its own handshake over a fresh `AgentLink::direct`.
	pub async fn open_bi(&self) -> AppResult<(SendStream, RecvStream)> {
		match &self.transport {
			Transport::Direct(conn) => conn
				.open_bi()
				.await
				.map_err(|e| AppError::msg(format!("open stream: {e}"))),
			Transport::Relay { relay, agent } => {
				let (mut send, recv) = relay
					.open_bi()
					.await
					.map_err(|e| AppError::msg(format!("open relay stream: {e}")))?;
				write_frame(&mut send, &RelayRequest::Route { agent: agent.clone() }).await?;
				Ok((send, recv))
			}
		}
	}

	/// Resolve the transport for a new authenticated stream: prefer a healthy direct
	/// upgrade, otherwise the base transport. Returns the opened raw stream halves plus
	/// the capability token and session key that path's handshake produced.
	async fn open_raw(&self) -> AppResult<(SendStream, RecvStream, Arc<str>, Arc<SessionKey>)> {
		if let Some(up) = self.healthy_upgrade() {
			let (send, recv) = up
				.conn
				.open_bi()
				.await
				.map_err(|e| AppError::msg(format!("open direct stream: {e}")))?;
			return Ok((send, recv, up.token, up.session_key));
		}
		let token = self
			.token
			.clone()
			.ok_or_else(|| AppError::msg("link is not authenticated (no capability token)"))?;
		let session_key = self
			.session_key
			.clone()
			.ok_or_else(|| AppError::msg("link has no end-to-end session (not authenticated)"))?;
		let (send, recv) = self.open_bi().await?;
		Ok((send, recv, token, session_key))
	}

	/// Open a non-handshake stream and bring it up to the point a payload can be
	/// written: pick the best path, announce `open` (plaintext, so the agent can route
	/// it), then wrap it in the end-to-end record layer and send the capability token
	/// *through* the encryption. Everything the caller writes/reads afterward is
	/// AEAD-sealed. Centralizes the "every post-handshake stream is authenticated and
	/// encrypted" invariant — and the direct-vs-relay path choice — in one place.
	pub async fn open_authenticated(&self, open: StreamOpen) -> AppResult<(SecureQuicSend, SecureQuicRecv)> {
		let (mut send, recv, token, key) = self.open_raw().await?;
		write_frame(&mut send, &open).await?;
		let (mut send, recv) = e2e::open_secure_controller(send, recv, &key).await?;
		write_frame(
			&mut send,
			&StreamAuth {
				token: token.to_string(),
			},
		)
		.await?;
		Ok((send, recv))
	}

	/// The agent's source address — known only for a directly-dialed connection.
	pub fn remote_address(&self) -> Option<SocketAddr> {
		match &self.transport {
			Transport::Direct(conn) => Some(conn.remote_address()),
			Transport::Relay { .. } => None,
		}
	}

	pub fn is_relay(&self) -> bool {
		matches!(self.transport, Transport::Relay { .. })
	}

	/// Drop the connection (direct only; the shared relay connection stays up). Also
	/// tears down any upgraded direct path.
	pub fn close(&self) {
		self.clear_upgrade();
		if let Transport::Direct(conn) = &self.transport {
			conn.close(0u32.into(), b"removed");
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use libretether_protocol::tls;
	use quinn::Endpoint;
	use std::net::Ipv4Addr;

	fn key() -> SessionKey {
		SessionKey::derive(&[9u8; 32], b"link-test")
	}

	/// A connected loopback QUIC pair; endpoints are returned so the caller keeps them
	/// (and the connections) alive.
	async fn loopback() -> (Endpoint, Connection, Endpoint, Connection) {
		tls::install_crypto_provider();
		let (cert, key) = tls::self_signed();
		let server_ep = Endpoint::server(tls::server_config(cert, key), (Ipv4Addr::LOCALHOST, 0).into()).unwrap();
		let addr = server_ep.local_addr().unwrap();
		let client_ep = tls::client_endpoint(addr).unwrap();
		let accept = {
			let ep = server_ep.clone();
			tokio::spawn(async move { ep.accept().await.unwrap().accept().unwrap().await.unwrap() })
		};
		let client_conn = client_ep.connect(addr, "libretether.local").unwrap().await.unwrap();
		let server_conn = accept.await.unwrap();
		(server_ep, server_conn, client_ep, client_conn)
	}

	// The upgrade state machine: a relay link with no direct path uses the relay; an
	// installed healthy direct path is preferred; and when it drops the link falls back
	// to the relay on its own (no external monitor needed).
	#[tokio::test]
	async fn prefers_a_healthy_direct_upgrade_then_falls_back_when_it_drops() {
		let (_re, relay_conn, _rce, _relay_client) = loopback().await;
		let link = AgentLink::relay(relay_conn, "AGENT".into()).with_session("relay-tok".into(), key());
		assert!(!link.is_upgraded(), "no direct path yet — new streams use the relay");

		// Install a direct connection as the upgrade.
		let (_de, direct_server, _dce, direct_client) = loopback().await;
		link.set_upgrade(direct_client.clone(), "direct-tok".into(), key());
		assert!(link.is_upgraded(), "a healthy direct path is preferred");

		// The direct connection drops → the link falls back to the relay automatically.
		direct_server.close(0u32.into(), b"bye");
		direct_client.closed().await;
		assert!(!link.is_upgraded(), "a dropped direct path falls back to the relay");
		assert!(
			!link.clear_upgrade_for(&direct_client),
			"nothing left to clear once it has fallen back"
		);
	}

	// A close monitor must only tear down the upgrade if it is still *its* connection —
	// a newer punch that already replaced it must survive the older one's close.
	#[tokio::test]
	async fn clear_upgrade_for_only_clears_a_matching_connection() {
		let (_re, relay_conn, _rce, _rc) = loopback().await;
		let link = AgentLink::relay(relay_conn, "AGENT".into()).with_session("relay-tok".into(), key());

		let (_oe, _os, _oce, old_direct) = loopback().await;
		let (_ne, _ns, _nce, new_direct) = loopback().await;
		link.set_upgrade(old_direct.clone(), "old".into(), key());
		// A newer punch replaces the upgrade with a different connection.
		link.set_upgrade(new_direct.clone(), "new".into(), key());

		// The *old* connection's monitor must not clear the newer upgrade.
		assert!(!link.clear_upgrade_for(&old_direct), "stale monitor is a no-op");
		assert!(link.is_upgraded(), "the newer direct path survives");
		// The current connection's monitor does clear it.
		assert!(link.clear_upgrade_for(&new_direct));
		assert!(!link.is_upgraded());
	}

	// A direct link (Tailscale/Direct mode) has no relay to upgrade off, so upgrade
	// operations are inert.
	#[tokio::test]
	async fn a_direct_link_is_never_upgradable() {
		let (_re, conn, _ce, _cc) = loopback().await;
		let link = AgentLink::direct(conn).with_session("tok".into(), key());
		let (_de, _ds, _dce, direct) = loopback().await;
		link.set_upgrade(direct.clone(), "d".into(), key());
		assert!(!link.is_upgraded(), "a direct link ignores upgrades");
		assert!(!link.clear_upgrade_for(&direct));
	}
}
