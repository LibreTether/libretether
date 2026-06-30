//! Framing for the optional relay server (`libretether-relay`).
//!
//! In relay mode neither the controller nor the agents are reachable, so both
//! dial *out* to the relay. The relay authenticates each side (owner secret for
//! the controller, agent secret for agents), tracks agents by their Ed25519
//! public key, and pipes streams between the controller and the addressed agent.
//! Everything inside those streams — the Ed25519 handshake, control RPCs, the
//! live session, and TCP tunnels — is end-to-end between controller and agent;
//! the relay only forwards bytes.

use serde::{Deserialize, Serialize};

use crate::crypto::Identity;
use crate::frame::{read_frame_capped, write_frame, MAX_CONTROL_FRAME};

/// Which side of the relay a client is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayRole {
	Controller,
	Agent,
}

/// First frame a client sends on its control stream to the relay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayHello {
	pub role: RelayRole,
	/// Owner secret (controller) or agent secret (agent).
	pub secret: String,
	/// Ed25519 public key — the agent's routing key (and identity).
	pub public_key: String,
}

/// Relay → client, after a valid secret: a nonce the client must sign with the
/// private key for the `public_key` it presented. This proves possession of the
/// key, so a holder of the (shared) agent secret cannot register under another
/// agent's public key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayChallenge {
	pub nonce: String,
}

/// Client → relay: the signature over [`RelayChallenge::nonce`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayProof {
	pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayAck {
	pub accepted: bool,
	pub reason: Option<String>,
}

/// Relay → controller presence notifications, written on the controller's
/// control stream after the ack.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum RelayEvent {
	AgentOnline {
		public_key: String,
	},
	AgentOffline {
		public_key: String,
	},
	/// Application-level liveness ping the relay emits periodically. QUIC
	/// keep-alives only prove the transport is up; a heartbeat proves the relay's
	/// routing loop is still servicing this controller, so a wedged relay (process
	/// alive, QUIC answering, but no longer forwarding) is detected by a read
	/// timeout on the controller instead of stranding every agent as offline.
	Heartbeat,
}

/// First frame the controller writes on each stream it opens to the relay.
///
/// Most streams are [`Self::Route`] — the relay strips this header and pipes
/// everything after it verbatim to (and from) the addressed agent, end-to-end.
/// [`Self::FetchLogs`] is the exception: the relay serves it itself, replying with
/// its own recent log lines (a [`crate::LogsResult`]) so an operator can read the
/// relay's activity from the controller's Logs page without shelling into the relay
/// host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum RelayRequest {
	/// Pipe this stream to the agent named by its Ed25519 public key.
	Route { agent: String },
	/// Return the relay's own recent log lines (oldest first). With `after_seq` set,
	/// only lines recorded after that cursor are returned (the controller polls
	/// incrementally and passes back the previous response's `next_seq`); `None`
	/// returns all retained lines. Answered by the relay, not forwarded to an agent.
	FetchLogs { after_seq: Option<u64> },
}

/// The client side of the relay handshake, shared by the agent and the
/// controller: open a control stream, present our secret + public key, prove we
/// hold the private key by signing the relay's nonce, and read the verdict.
///
/// On success returns the hello stream's `(send, recv)` halves — the controller
/// keeps reading presence events on `recv`; the agent simply drops them. A clean
/// rejection (or any I/O error) is surfaced as an [`std::io::Error`] so both the
/// agent's `anyhow` and the controller's error type can absorb it with `?`.
pub async fn client_handshake(
	conn: &quinn::Connection,
	role: RelayRole,
	secret: &str,
	identity: &Identity,
) -> std::io::Result<(quinn::SendStream, quinn::RecvStream)> {
	let (mut send, mut recv) = conn
		.open_bi()
		.await
		.map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, format!("open relay stream: {e}")))?;
	write_frame(
		&mut send,
		&RelayHello {
			role,
			secret: secret.to_string(),
			public_key: identity.public_b64(),
		},
	)
	.await?;
	// Prove possession of the presented key so a holder of the (shared) secret
	// can't register under another peer's public key.
	let challenge: RelayChallenge = read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await?;
	write_frame(
		&mut send,
		&RelayProof {
			signature: identity.sign_b64(challenge.nonce.as_bytes()),
		},
	)
	.await?;
	let ack: RelayAck = read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await?;
	if !ack.accepted {
		return Err(std::io::Error::new(
			std::io::ErrorKind::PermissionDenied,
			format!("relay rejected connection: {}", ack.reason.unwrap_or_default()),
		));
	}
	Ok((send, recv))
}
