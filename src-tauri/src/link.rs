//! How the controller reaches an agent — directly (the agent dialed us) or
//! through the relay. Everything that talks to an agent goes through
//! [`AgentLink::open_bi`], so the control plane is identical in every mode.

use std::net::SocketAddr;

use libretether_protocol::frame::write_frame;
use libretether_protocol::relay::RouteTo;
use quinn::{Connection, RecvStream, SendStream};

use crate::error::{AppError, AppResult};

#[derive(Clone)]
pub enum AgentLink {
	/// The agent dialed us directly (Tailscale / Direct mode).
	Direct(Connection),
	/// Reach the agent through the relay, addressed by its public key.
	Relay { relay: Connection, agent: String },
}

impl AgentLink {
	/// Open a fresh bidirectional stream to the agent. In relay mode this opens
	/// a stream to the relay prefixed with a routing header; everything after is
	/// piped to the agent verbatim.
	pub async fn open_bi(&self) -> AppResult<(SendStream, RecvStream)> {
		match self {
			AgentLink::Direct(conn) => conn
				.open_bi()
				.await
				.map_err(|e| AppError::msg(format!("open stream: {e}"))),
			AgentLink::Relay { relay, agent } => {
				let (mut send, recv) = relay
					.open_bi()
					.await
					.map_err(|e| AppError::msg(format!("open relay stream: {e}")))?;
				write_frame(&mut send, &RouteTo { agent: agent.clone() }).await?;
				Ok((send, recv))
			}
		}
	}

	/// The agent's source address — known only for direct connections.
	pub fn remote_address(&self) -> Option<SocketAddr> {
		match self {
			AgentLink::Direct(conn) => Some(conn.remote_address()),
			AgentLink::Relay { .. } => None,
		}
	}

	pub fn is_relay(&self) -> bool {
		matches!(self, AgentLink::Relay { .. })
	}

	/// Drop the connection (direct only; the shared relay connection stays up).
	pub fn close(&self) {
		if let AgentLink::Direct(conn) = self {
			conn.close(0u32.into(), b"removed");
		}
	}
}
