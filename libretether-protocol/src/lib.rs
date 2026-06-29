//! Shared wire protocol between the LibreTether controller and its agents.
//!
//! Topology: the controller is the QUIC *server*; each agent *dials in* over
//! the tailnet and holds the connection open. Authentication is a
//! challenge/response — the controller issues a random nonce, the agent signs
//! it with its Ed25519 key, and the controller checks the signature against the
//! public key it recorded at enrollment. A one-time enrollment token binds the
//! very first connection.
//!
//! Once authenticated, the controller drives the agent by opening a fresh
//! bidirectional QUIC stream per request (see [`ControlRequest`]). Live screen
//! control uses a single long-lived bidirectional stream that is full-duplex:
//! the controller writes [`SessionClient`] events while the agent writes
//! [`SessionServer`] frames (see the `session` module types below).

pub mod crypto;
pub mod frame;
pub mod relay;
pub mod secret;
pub mod tls;

use serde::{Deserialize, Serialize};

/// ALPN protocol identifier negotiated during the QUIC/TLS handshake.
pub const ALPN: &[u8] = b"libretether/1";

/// Bumped whenever the wire format changes incompatibly. v2 added mutual
/// authentication (the controller proves its identity to the agent via
/// `Challenge.controller_key` + `HelloAck.controller_sig`); v3 made the version
/// check mutual too — `Challenge.protocol` lets the agent reject a
/// version-mismatched controller, mirroring the controller's `Hello.protocol`
/// check, so a skew fails closed on *both* ends (no compatibility shims).
pub const PROTOCOL_VERSION: u32 = 3;

/// Default UDP port the controller listens on for incoming agents.
pub const DEFAULT_PORT: u16 = 47600;

/// Wall-clock an [`ControlRequest::Exec`] runs on the agent before it is killed,
/// when the controller doesn't specify a `timeout_secs`. The controller sizes its
/// own response timeout from these so a legitimate long exec isn't cut off while a
/// wedged agent still can't hang the caller forever.
pub const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 30;
/// Hard upper bound on an [`ControlRequest::Exec`]'s wall-clock on the agent. The
/// agent clamps every request to this.
pub const MAX_EXEC_TIMEOUT_SECS: u64 = 600;

// ---------------------------------------------------------------- handshake

/// Basic identification of the machine an agent runs on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
	pub hostname: String,
	pub os: String,
	pub arch: String,
	pub username: String,
}

/// Server → agent, first message on the handshake stream: a nonce for the agent
/// to sign, plus the controller's own Ed25519 public key so the agent can verify
/// it is talking to the controller it enrolled with (see `HelloAck.controller_sig`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Challenge {
	/// The controller's [`PROTOCOL_VERSION`]. The agent rejects a mismatch with a
	/// clear "upgrade both ends" error before issuing any capability token, so a
	/// version skew fails closed instead of failing in confusing ways downstream.
	/// Mandatory (no serde default) — a frame missing it is a protocol violation.
	pub protocol: u32,
	pub nonce: String,
	/// Base64 Ed25519 public key identifying the controller. The agent pins this
	/// at enrollment and rejects any controller whose key/signature don't match.
	/// Mandatory: a v2+ controller always sends it, so a frame missing it is a
	/// protocol violation and is rejected at parse time (no compatibility shim).
	pub controller_key: String,
}

/// Agent → server: proves identity and (on first connect) enrolls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
	pub protocol: u32,
	/// Present only until the agent has been enrolled with the controller.
	pub enrollment_token: Option<String>,
	/// Base64 Ed25519 public key — the agent's stable identity.
	pub public_key: String,
	/// Base64 signature over the challenge nonce.
	pub signature: String,
	/// A fresh nonce the controller must sign back, so the agent can authenticate
	/// the controller in turn (mutual auth). Mandatory — see [`Challenge::controller_key`].
	pub agent_nonce: String,
	pub host: HostInfo,
	pub agent_version: String,
}

/// Agent → controller, the final handshake message on success: a per-connection
/// capability token the controller must present (see [`StreamAuth`]) on every
/// control/session/tunnel stream it later opens.
///
/// It is sent only *after* the agent has verified the controller's identity, so
/// a party that cannot complete the mutual handshake — e.g. someone who merely
/// holds the relay's owner secret but not the controller's private key — never
/// learns it, and therefore cannot drive the agent through the relay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGrant {
	pub token: String,
}

/// Controller → agent, written right after [`StreamOpen`] on every non-handshake
/// stream: the capability token from the [`SessionGrant`] of the handshake that
/// authenticated this controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamAuth {
	pub token: String,
}

/// Server → agent: the verdict of the handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloAck {
	pub accepted: bool,
	pub reason: Option<String>,
	/// The controller-assigned client id, echoed back so the agent can log it.
	pub client_id: Option<String>,
	/// Base64 signature over the agent's `agent_nonce`, made with the controller's
	/// identity key. The agent verifies this against the pinned controller key
	/// before honouring any control/session/tunnel stream. Mandatory — see
	/// [`Challenge::controller_key`]. (On a rejection it is an empty string, but the
	/// field is always present on the wire.)
	pub controller_sig: String,
}

// ---------------------------------------------------------------- streams

/// The first frame on every bidirectional stream the controller opens, so the
/// agent knows how to handle what follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stream", rename_all = "snake_case")]
pub enum StreamOpen {
	/// Auth handshake: a [`Challenge`] follows from the controller, then the
	/// agent replies with [`Hello`] and reads a [`HelloAck`].
	Handshake,
	/// A single [`ControlRequest`]/[`ControlResponse`] exchange.
	Control,
	/// A live screen-control session (full-duplex [`SessionClient`]/[`SessionServer`]).
	Session,
	/// A raw TCP tunnel: the agent connects to `127.0.0.1:port` and pipes bytes
	/// both ways (used to reach the client's RDP/SSH server through the relay).
	Tunnel { port: u16 },
}

// ---------------------------------------------------------------- control RPC

/// A one-shot request the controller sends on a dedicated bidirectional stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlRequest {
	Ping,
	Status,
	Exec {
		program: String,
		#[serde(default)]
		args: Vec<String>,
		timeout_secs: Option<u64>,
	},
	Screenshot {
		display: Option<u32>,
	},
	/// Turn on an RDP server on the client and return how to reach it.
	EnableRdp,
}

/// The matching response, written back on the same stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlResponse {
	Pong,
	Status(AgentStatus),
	Exec(ExecResult),
	Screenshot(ScreenshotResult),
	Rdp(RdpInfo),
	Error { message: String },
}

/// How to reach the RDP server the agent just enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RdpInfo {
	/// Which server is backing RDP, e.g. "gnome-remote-desktop" or "windows".
	pub backend: String,
	/// Optional address override; when absent, the controller dials the address
	/// the agent connected from (its tailnet IP).
	pub address: Option<String>,
	pub port: u16,
	pub username: String,
	/// Present when the agent manages its own credentials (gnome-remote-desktop);
	/// absent when the client's existing OS credentials are used (Windows).
	pub password: Option<String>,
	/// A human-readable hint to surface if the connection needs attention.
	pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatus {
	pub host: HostInfo,
	pub agent_version: String,
	/// How long the agent process itself has been running.
	pub uptime_secs: u64,
	/// Unix seconds when the agent process started.
	pub started_at: u64,
	/// Unix seconds the machine booted, when known.
	pub boot_time_secs: Option<u64>,
	/// Number of attached displays the agent can see.
	pub displays: u32,
	/// The agent's tailnet IPv4, when on Tailscale — used as the address the
	/// controller dials for RDP/SSH.
	pub tailscale_ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
	pub code: Option<i32>,
	pub stdout: String,
	pub stderr: String,
	pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenshotResult {
	pub display: u32,
	pub width: u32,
	pub height: u32,
	/// PNG bytes, base64-encoded.
	pub png_base64: String,
}

// ---------------------------------------------------------------- live session

/// Quality/format knobs for a live screen-control session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
	pub display: u32,
	/// JPEG quality, 1–100.
	pub quality: u8,
	/// Upper bound on frames per second the agent should emit.
	pub max_fps: u8,
}

impl Default for SessionConfig {
	fn default() -> Self {
		Self {
			display: 0,
			quality: 70,
			max_fps: 20,
		}
	}
}

/// Controller → agent, multiplexed on the session stream.
///
/// Tagged with `kind` (not `t`): the `Input` variant wraps [`InputEvent`], which
/// is itself internally tagged with `t`. Sharing the tag key would make
/// `Input(InputEvent)` serialize a map with two `t` fields, and deserialization
/// then fails with "duplicate field `t`" — silently breaking all input.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionClient {
	Start(SessionConfig),
	Input(InputEvent),
	/// Ask for a fresh full frame (e.g. after the UI resized).
	Refresh,
	Stop,
}

/// Agent → controller, multiplexed on the session stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum SessionServer {
	/// Sent once at start and whenever the captured geometry changes.
	Meta {
		display: u32,
		width: u32,
		height: u32,
	},
	Frame(Frame),
	Error {
		message: String,
	},
}

/// A single captured frame. Coordinates and sizes are in source pixels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
	pub seq: u64,
	pub width: u32,
	pub height: u32,
	pub encoding: FrameEncoding,
	/// Frame payload (JPEG/PNG bytes), base64-encoded.
	pub data_base64: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameEncoding {
	Jpeg,
	Png,
}

/// Pointer buttons the controller can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
	Left,
	Right,
	Middle,
}

/// An input event the controller injects on the remote machine. Mouse
/// coordinates are normalized 0.0–1.0 of the captured display so they survive
/// resolution differences between the two ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum InputEvent {
	MouseMove {
		x: f64,
		y: f64,
	},
	MouseButton {
		button: MouseButton,
		pressed: bool,
	},
	MouseScroll {
		dx: i32,
		dy: i32,
	},
	/// A physical key identified by its W3C `KeyboardEvent.code` (e.g. "KeyA",
	/// "Enter", "ShiftLeft"). The agent maps it to a platform keysym.
	Key {
		code: String,
		pressed: bool,
	},
	/// Committed text (IME, paste) injected as a unit.
	Text {
		text: String,
	},
}

#[cfg(test)]
mod tests {
	use super::*;

	// `SessionClient` is tagged with `kind` and the wrapped `InputEvent` with `t`.
	// If they shared a tag key the map would carry two of it and deserialization
	// would fail with "duplicate field". This locks the distinct tags in.
	#[test]
	fn session_client_input_uses_distinct_tags_and_round_trips() {
		let ev = SessionClient::Input(InputEvent::MouseMove { x: 0.5, y: 0.25 });
		let json = serde_json::to_value(&ev).unwrap();
		assert_eq!(json["kind"], "input");
		assert_eq!(json["t"], "mouse_move");

		let back: SessionClient = serde_json::from_value(json).unwrap();
		assert!(matches!(back, SessionClient::Input(InputEvent::MouseMove { .. })));
	}

	// The v2 mutual-auth fields are mandatory. A handshake frame missing any of
	// them is a protocol violation and must be rejected at parse time (fail
	// closed), never silently defaulted — the project keeps no compatibility
	// shims. This is the regression guard for that rule.
	#[test]
	fn handshake_frames_reject_missing_mutual_auth_fields() {
		// Challenge without `controller_key` (and without `protocol`).
		assert!(serde_json::from_str::<Challenge>(r#"{"protocol":3,"nonce":"n"}"#).is_err());
		// Challenge without `protocol` — the v3 version field is mandatory too.
		assert!(serde_json::from_str::<Challenge>(r#"{"nonce":"n","controller_key":"ck"}"#).is_err());
		// HelloAck without `controller_sig` (the Option fields may be absent).
		assert!(serde_json::from_str::<HelloAck>(r#"{"accepted":true}"#).is_err());
		// Hello without `agent_nonce`.
		let hello_missing = r#"{"protocol":2,"public_key":"k","signature":"s",
			"host":{"hostname":"h","os":"o","arch":"a","username":"u"},"agent_version":"1"}"#;
		assert!(serde_json::from_str::<Hello>(hello_missing).is_err());
	}

	#[test]
	fn complete_handshake_frames_round_trip() {
		let challenge = Challenge {
			protocol: PROTOCOL_VERSION,
			nonce: "n".into(),
			controller_key: "ck".into(),
		};
		let back: Challenge = round_trip(&challenge);
		assert_eq!(back.protocol, PROTOCOL_VERSION);
		assert_eq!(back.nonce, "n");
		assert_eq!(back.controller_key, "ck");

		let hello = Hello {
			protocol: PROTOCOL_VERSION,
			enrollment_token: Some("tok".into()),
			public_key: "pk".into(),
			signature: "sig".into(),
			agent_nonce: "an".into(),
			host: HostInfo {
				hostname: "h".into(),
				os: "linux".into(),
				arch: "x86_64".into(),
				username: "u".into(),
			},
			agent_version: "1.2.3".into(),
		};
		let back: Hello = round_trip(&hello);
		assert_eq!(back.agent_nonce, "an");
		assert_eq!(back.enrollment_token.as_deref(), Some("tok"));

		let ack = HelloAck {
			accepted: true,
			reason: None,
			client_id: Some("cid".into()),
			controller_sig: "csig".into(),
		};
		let back: HelloAck = round_trip(&ack);
		assert!(back.accepted);
		assert_eq!(back.controller_sig, "csig");

		// An enrolled agent omits the token; that must still round-trip.
		let enrolled = Hello {
			enrollment_token: None,
			..hello
		};
		assert!(round_trip::<Hello>(&enrolled).enrollment_token.is_none());
	}

	#[test]
	fn control_request_and_response_variants_round_trip() {
		for req in [
			ControlRequest::Ping,
			ControlRequest::Status,
			ControlRequest::Exec {
				program: "echo".into(),
				args: vec!["hi".into()],
				timeout_secs: Some(5),
			},
			ControlRequest::Screenshot { display: Some(1) },
			ControlRequest::EnableRdp,
		] {
			let json = serde_json::to_string(&req).unwrap();
			let back: ControlRequest = serde_json::from_str(&json).unwrap();
			// Tag survives the round-trip.
			assert_eq!(
				serde_json::to_value(&req).unwrap()["kind"],
				serde_json::to_value(&back).unwrap()["kind"]
			);
		}

		// `args` defaults to empty when omitted; `timeout_secs` is optional.
		let exec: ControlRequest = serde_json::from_str(r#"{"kind":"exec","program":"ls"}"#).unwrap();
		assert!(matches!(exec, ControlRequest::Exec { args, timeout_secs: None, .. } if args.is_empty()));

		let resp = ControlResponse::Error { message: "boom".into() };
		let back: ControlResponse = round_trip(&resp);
		assert!(matches!(back, ControlResponse::Error { message } if message == "boom"));
	}

	#[test]
	fn stream_open_round_trips_including_tunnel_port() {
		for open in [
			StreamOpen::Handshake,
			StreamOpen::Control,
			StreamOpen::Session,
			StreamOpen::Tunnel { port: 3389 },
		] {
			assert_eq!(round_trip::<StreamOpen>(&open), open);
		}
		let tunnel: StreamOpen = serde_json::from_str(r#"{"stream":"tunnel","port":22}"#).unwrap();
		assert_eq!(tunnel, StreamOpen::Tunnel { port: 22 });
	}

	#[test]
	fn session_messages_round_trip() {
		let meta = SessionServer::Meta {
			display: 0,
			width: 1920,
			height: 1080,
		};
		assert!(matches!(
			round_trip::<SessionServer>(&meta),
			SessionServer::Meta { width: 1920, .. }
		));

		let start = SessionClient::Start(SessionConfig::default());
		assert!(matches!(round_trip::<SessionClient>(&start), SessionClient::Start(_)));

		for ev in [
			InputEvent::MouseMove { x: 0.1, y: 0.9 },
			InputEvent::MouseButton {
				button: MouseButton::Right,
				pressed: true,
			},
			InputEvent::MouseScroll { dx: -1, dy: 2 },
			InputEvent::Key {
				code: "KeyA".into(),
				pressed: false,
			},
			InputEvent::Text { text: "hi".into() },
		] {
			let wrapped = SessionClient::Input(ev);
			assert!(matches!(round_trip::<SessionClient>(&wrapped), SessionClient::Input(_)));
		}
	}

	fn round_trip<T: Serialize + DeserializeOwned>(value: &T) -> T {
		serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap()
	}

	use serde::de::DeserializeOwned;
}
