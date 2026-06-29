//! How the controller reaches an agent — directly (the agent dialed us) or
//! through the relay. Everything that talks to an agent goes through
//! [`AgentLink::open_bi`], so the control plane is identical in every mode.
//!
//! After the mutual handshake the agent issues a per-connection capability token
//! (see the protocol's `SessionGrant`); the link carries it and stamps every
//! non-handshake stream with it via [`AgentLink::authenticate`], so the agent can
//! tell a verified controller's streams apart from anything else the relay might
//! route to it.

use std::net::SocketAddr;
use std::sync::Arc;

use libretether_protocol::frame::write_frame;
use libretether_protocol::relay::RouteTo;
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

#[derive(Clone)]
pub struct AgentLink {
	transport: Transport,
	/// Capability token from the handshake's `SessionGrant`; `None` until the
	/// handshake completes (the handshake stream itself carries no token).
	token: Option<Arc<str>>,
}

impl AgentLink {
	pub fn direct(conn: Connection) -> Self {
		Self {
			transport: Transport::Direct(conn),
			token: None,
		}
	}

	pub fn relay(relay: Connection, agent: String) -> Self {
		Self {
			transport: Transport::Relay { relay, agent },
			token: None,
		}
	}

	/// Return a copy of this link that stamps its streams with `token`.
	pub fn with_token(&self, token: String) -> Self {
		Self {
			transport: self.transport.clone(),
			token: Some(Arc::from(token.as_str())),
		}
	}

	/// Open a fresh bidirectional stream to the agent. In relay mode this opens
	/// a stream to the relay prefixed with a routing header; everything after is
	/// piped to the agent verbatim.
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
				write_frame(&mut send, &RouteTo { agent: agent.clone() }).await?;
				Ok((send, recv))
			}
		}
	}

	/// Open a non-handshake stream and bring it up to the point a payload can be
	/// written: open the bi stream, announce `open`, and stamp it with the
	/// capability token. Centralizes the "every post-handshake stream the controller
	/// opens must carry the token" invariant so a new stream type can't forget it.
	pub async fn open_authenticated(&self, open: StreamOpen) -> AppResult<(SendStream, RecvStream)> {
		let (mut send, recv) = self.open_bi().await?;
		write_frame(&mut send, &open).await?;
		self.authenticate(&mut send).await?;
		Ok((send, recv))
	}

	/// Stamp a non-handshake stream with the capability token. Call right after
	/// writing the `StreamOpen` frame and before the payload. No-op if no token
	/// is set (only the handshake link, which the agent exempts).
	pub async fn authenticate(&self, send: &mut SendStream) -> AppResult<()> {
		if let Some(token) = &self.token {
			write_frame(
				send,
				&StreamAuth {
					token: token.to_string(),
				},
			)
			.await?;
		}
		Ok(())
	}

	/// The agent's source address — known only for direct connections.
	pub fn remote_address(&self) -> Option<SocketAddr> {
		match &self.transport {
			Transport::Direct(conn) => Some(conn.remote_address()),
			Transport::Relay { .. } => None,
		}
	}

	pub fn is_relay(&self) -> bool {
		matches!(self.transport, Transport::Relay { .. })
	}

	/// Drop the connection (direct only; the shared relay connection stays up).
	pub fn close(&self) {
		if let Transport::Direct(conn) = &self.transport {
			conn.close(0u32.into(), b"removed");
		}
	}
}
