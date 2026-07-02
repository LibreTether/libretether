//! Framing for the optional relay server (`libretether-relay`).
//!
//! In relay mode neither the controller nor the agents are reachable, so both
//! dial *out* to the relay. The relay is **multi-tenant**: it holds a set of
//! independent tenants, each with its own owner secret (authenticates that
//! tenant's controller) and agent secret (authenticates that tenant's agents).
//! A presented secret identifies the tenant, so routing is isolated — a
//! controller only ever sees the agents that dialed in under *its* tenant's
//! agent secret. The relay tracks each tenant's agents by their Ed25519 public
//! key and pipes streams between that tenant's controller and the addressed
//! agent. Everything inside those streams — the Ed25519 handshake, control RPCs,
//! the live session, and TCP tunnels — is end-to-end between controller and
//! agent; the relay only forwards bytes.
//!
//! Tenants are minted through an [`RelayRole::Admin`] channel: a client that
//! holds the relay's admin secret (or any client, when the relay has open
//! registration enabled) provisions a tenant and receives its freshly-generated
//! secrets ([`AdminRequest`] / [`AdminResponse`]).

use serde::{Deserialize, Serialize};

use crate::crypto::Identity;
use crate::frame::{read_frame_capped, write_frame, MAX_CONTROL_FRAME};

/// Which side of the relay a client is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayRole {
	Controller,
	Agent,
	/// A not-yet-enrolled machine joining a pairing mailbox by nameplate (see
	/// [`crate::pairing`]). It presents no secret — the relay only matches it to a
	/// controller's open pairing slot and pipes the two together; the PAKE over that
	/// pipe is the real authentication, so the relay never trusts this peer.
	Pairing,
	/// A provisioning client: it authenticates with the relay's **admin secret** and
	/// then issues [`AdminRequest`]s to mint / list / revoke tenants. When the relay
	/// has open registration enabled a client with a non-matching secret is still
	/// admitted, but only to `Provision` its own tenant (never to list or revoke
	/// others).
	Admin,
}

/// First frame a client sends on its control stream to the relay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayHello {
	pub role: RelayRole,
	/// The tenant owner secret (controller), the tenant agent secret (agent), or the
	/// relay admin secret (admin). Empty for the `Pairing` role, which carries no
	/// secret — and permitted empty for `Admin` when the relay allows open
	/// registration (such a client can only provision its own tenant).
	pub secret: String,
	/// Ed25519 public key — the agent's routing key (and identity). Empty for the
	/// `Pairing` role (the joining machine isn't trusted by key yet).
	pub public_key: String,
	/// The pairing nameplate, set only for the `Pairing` role: the relay-visible
	/// routing id that matches this joiner to a controller's open pairing slot. The
	/// code's secret half is never sent — see [`crate::pairing`].
	#[serde(default)]
	pub nameplate: Option<String>,
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

/// Relay → agent signals, pushed on the agent's hello stream (the same stream the
/// agent opened to register, which it otherwise leaves idle). Symmetric to the
/// controller's [`RelayEvent`] channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum RelaySignal {
	/// The controller wants to establish a direct peer-to-peer path (NAT hole-punch).
	/// `controller_addr` is the controller's reflexive address as the relay observed
	/// it; the agent dials it directly from its relay socket while the controller
	/// punches its own NAT open. `rendezvous` correlates the attempt in logs — the
	/// direct connection is still authenticated by the normal mutual handshake, so a
	/// bogus address just fails to authenticate and is dropped (fail-closed).
	Punch {
		controller_addr: String,
		rendezvous: String,
	},
}

/// The controller's response to a [`RelayRequest::Punch`]: where to expect the agent
/// (its reflexive address) and the rendezvous id the relay also handed the agent.
/// `peer_addr` is `None` when the relay can't broker the punch — the agent is offline
/// or the relay doesn't know its address — so the controller stays on the relay path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PunchResponse {
	pub peer_addr: Option<String>,
	pub rendezvous: String,
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
	/// Open a pairing mailbox under `nameplate`: the relay parks this stream until a
	/// `Pairing`-role peer joins with the same nameplate, then pipes the two together
	/// so the controller and the new machine can run the PAKE end-to-end (see
	/// [`crate::pairing`]). The relay only matches by nameplate and forwards bytes.
	OpenPairing { nameplate: String },
	/// Ask the relay to broker a peer-to-peer hole-punch to `agent`: it hands the
	/// agent the controller's reflexive address (over the agent's signal channel) and
	/// replies with a [`PunchResponse`] carrying the agent's reflexive address. Both
	/// then try to establish a direct QUIC path, upgrading off the relay when the punch
	/// succeeds. Answered by the relay, not forwarded to an agent.
	Punch { agent: String },
}

/// A newly-provisioned tenant's credentials, returned to the provisioning client.
/// The owner secret authenticates the tenant's controller; the agent secret is
/// baked into that tenant's deploy scripts. Both are generated by the relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantCredentials {
	pub tenant_id: String,
	pub name: String,
	pub owner_secret: String,
	pub agent_secret: String,
}

/// A tenant's public status, returned by [`AdminRequest::List`]. Never carries the
/// tenant's secrets — listing is a management view, not a credential dump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantInfo {
	pub tenant_id: String,
	pub name: String,
	/// Whether a controller is currently connected for this tenant.
	pub controller_online: bool,
	/// How many of this tenant's agents are currently registered.
	pub agents_online: usize,
}

/// A provisioning request sent by an [`RelayRole::Admin`] client on a stream it
/// opens after the handshake. The relay answers each with an [`AdminResponse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AdminRequest {
	/// Mint a new tenant with a human label and return its generated secrets.
	/// Allowed for a full admin and — when the relay has open registration on — for
	/// any admitted client.
	Provision { name: String },
	/// List every tenant's public status (no secrets). Full admin only.
	List,
	/// Remove a tenant, disconnecting its controller and agents. Full admin only.
	Revoke { tenant_id: String },
}

/// The relay's reply to an [`AdminRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AdminResponse {
	/// A tenant was minted; carries its credentials.
	Provisioned(TenantCredentials),
	/// The tenant list (response to [`AdminRequest::List`]).
	Tenants { tenants: Vec<TenantInfo> },
	/// A revoke completed; `existed` is false if no tenant had that id.
	Revoked { tenant_id: String, existed: bool },
	/// The request was refused — e.g. `List`/`Revoke` without full-admin rights, the
	/// tenant cap was hit, or a blank tenant name.
	Error { message: String },
}

/// The client side of the relay handshake, shared by the agent, the controller,
/// and an admin: open a control stream, present our secret + public key, prove we
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
			nameplate: None,
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

/// The not-yet-enrolled machine's side of joining a pairing mailbox: open a stream,
/// announce the nameplate, and return its halves to run the PAKE over. The relay
/// pipes this stream to the controller's matching open slot. There is no ack — if no
/// slot matches (wrong/expired nameplate), the relay resets the stream and the PAKE
/// read/write that follows fails, which the caller surfaces as a pairing error.
pub async fn pairing_join(
	conn: &quinn::Connection,
	nameplate: &str,
) -> std::io::Result<(quinn::SendStream, quinn::RecvStream)> {
	let (mut send, recv) = conn.open_bi().await.map_err(|e| {
		std::io::Error::new(
			std::io::ErrorKind::ConnectionRefused,
			format!("open pairing stream: {e}"),
		)
	})?;
	write_frame(
		&mut send,
		&RelayHello {
			role: RelayRole::Pairing,
			secret: String::new(),
			public_key: String::new(),
			nameplate: Some(nameplate.to_string()),
		},
	)
	.await?;
	Ok((send, recv))
}

/// The controller's side of asking the relay to broker a peer-to-peer hole-punch to
/// `agent`: open a stream, send [`RelayRequest::Punch`], and read the [`PunchResponse`]
/// telling it where to expect the agent. The relay answers this itself (it doesn't
/// route it to the agent). A `peer_addr` of `None` means the relay couldn't broker it
/// and the controller should stay on the relay path.
pub async fn request_punch(conn: &quinn::Connection, agent: &str) -> std::io::Result<PunchResponse> {
	let (mut send, mut recv) = conn
		.open_bi()
		.await
		.map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, format!("open punch stream: {e}")))?;
	write_frame(
		&mut send,
		&RelayRequest::Punch {
			agent: agent.to_string(),
		},
	)
	.await?;
	let _ = send.finish();
	read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await
}

/// Send one [`AdminRequest`] on a fresh stream and read the [`AdminResponse`]. The
/// connection must have completed [`client_handshake`] with [`RelayRole::Admin`]
/// first — the relay decides full-admin vs open-registration rights from the secret
/// presented there, so this just carries the request.
pub async fn admin_request(conn: &quinn::Connection, req: &AdminRequest) -> std::io::Result<AdminResponse> {
	let (mut send, mut recv) = conn
		.open_bi()
		.await
		.map_err(|e| std::io::Error::new(std::io::ErrorKind::ConnectionRefused, format!("open admin stream: {e}")))?;
	write_frame(&mut send, req).await?;
	let _ = send.finish();
	read_frame_capped(&mut recv, MAX_CONTROL_FRAME).await
}

#[cfg(test)]
mod tests {
	use super::*;

	fn round_trip<T: Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
		serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap()
	}

	#[test]
	fn punch_request_and_response_round_trip() {
		// The new Punch request tag survives, distinct from the routing variants.
		let req = RelayRequest::Punch {
			agent: "AGENT_KEY".into(),
		};
		assert_eq!(serde_json::to_value(&req).unwrap()["t"], "punch");
		assert!(matches!(round_trip(&req), RelayRequest::Punch { agent } if agent == "AGENT_KEY"));

		// A brokered response carries the peer's reflexive address and rendezvous id.
		let ok = PunchResponse {
			peer_addr: Some("203.0.113.4:47600".into()),
			rendezvous: "rv-1".into(),
		};
		let back = round_trip(&ok);
		assert_eq!(back.peer_addr.as_deref(), Some("203.0.113.4:47600"));
		assert_eq!(back.rendezvous, "rv-1");

		// `None` (relay can't broker) round-trips too, so the controller can tell
		// "stay on the relay" apart from a real address.
		assert!(round_trip(&PunchResponse {
			peer_addr: None,
			rendezvous: "rv-2".into(),
		})
		.peer_addr
		.is_none());
	}

	#[test]
	fn admin_frames_round_trip() {
		// The provisioning request tags are stable and distinct.
		let provision = AdminRequest::Provision { name: "team-a".into() };
		assert_eq!(serde_json::to_value(&provision).unwrap()["t"], "provision");
		assert!(matches!(round_trip(&provision), AdminRequest::Provision { name } if name == "team-a"));
		assert!(matches!(round_trip(&AdminRequest::List), AdminRequest::List));
		assert!(matches!(round_trip(&AdminRequest::Revoke { tenant_id: "t1".into() }),
			AdminRequest::Revoke { tenant_id } if tenant_id == "t1"));

		// A provisioned response carries the minted credentials verbatim.
		let creds = TenantCredentials {
			tenant_id: "t1".into(),
			name: "team-a".into(),
			owner_secret: "own".into(),
			agent_secret: "agt".into(),
		};
		let back = round_trip(&AdminResponse::Provisioned(creds.clone()));
		assert!(matches!(back, AdminResponse::Provisioned(c) if c == creds));

		// The list response omits secrets and preserves per-tenant status.
		let info = TenantInfo {
			tenant_id: "t1".into(),
			name: "team-a".into(),
			controller_online: true,
			agents_online: 3,
		};
		let listed = round_trip(&AdminResponse::Tenants {
			tenants: vec![info.clone()],
		});
		assert!(matches!(listed, AdminResponse::Tenants { tenants } if tenants == vec![info]));

		assert!(matches!(
			round_trip(&AdminResponse::Revoked {
				tenant_id: "t1".into(),
				existed: true
			}),
			AdminResponse::Revoked { existed: true, .. }
		));
	}

	#[test]
	fn punch_signal_round_trips() {
		let sig = RelaySignal::Punch {
			controller_addr: "198.51.100.9:47600".into(),
			rendezvous: "rv-3".into(),
		};
		assert_eq!(serde_json::to_value(&sig).unwrap()["t"], "punch");
		let RelaySignal::Punch {
			controller_addr,
			rendezvous,
		} = round_trip(&sig);
		assert_eq!(controller_addr, "198.51.100.9:47600");
		assert_eq!(rendezvous, "rv-3");
	}
}
