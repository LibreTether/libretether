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
//! [`SessionServer`] control messages interleaved with binary [`video`] frames
//! (see the session types below and the [`video`] module).

pub mod crypto;
pub mod frame;
pub mod pairing;
pub mod relay;
pub mod secret;
pub mod tls;
pub mod video;
mod wordlist;

use serde::{Deserialize, Serialize};

/// ALPN protocol identifier negotiated during the QUIC/TLS handshake.
pub const ALPN: &[u8] = b"libretether/1";

/// Bumped whenever the wire format changes incompatibly. v2 added mutual
/// authentication (the controller proves its identity to the agent via
/// `Challenge.controller_key` + `HelloAck.controller_sig`); v3 made the version
/// check mutual too — `Challenge.protocol` lets the agent reject a
/// version-mismatched controller, mirroring the controller's `Hello.protocol`
/// check, so a skew fails closed on *both* ends (no compatibility shims); v4
/// added the [`ControlRequest::FetchLogs`] RPC so the controller can pull an
/// agent's recent log buffer; v5 replaced the JSON+base64 live-session `Frame`
/// with binary, tile-delta [`video`] frames and added `SessionConfig.scale` +
/// the [`SessionClient::Configure`] live-quality control; v6 replaced the
/// per-tile baseline-JPEG video format with a real inter-frame H.264 stream
/// (decoded by WebCodecs on the controller) and swapped `SessionConfig.quality`
/// (JPEG 1–100) for `SessionConfig.bitrate_kbps`.
pub const PROTOCOL_VERSION: u32 = 6;

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
	/// Start (if needed) the agent's built-in SSH server and return how to reach it
	/// — a loopback port plus an ephemeral private key. Used as a fallback so SSH
	/// works even on a client with no system SSH server installed.
	EnableSsh,
	/// Return the agent's recent in-memory log lines (most recent last), capped at
	/// `max_lines` when set. Lets the controller surface a client's agent log
	/// without shelling into the machine.
	FetchLogs {
		max_lines: Option<u32>,
	},
	/// Check whether something is listening on `127.0.0.1:port` on the agent host.
	/// The controller uses this to confirm a client's SSH server is actually up
	/// before launching a terminal. Having the agent probe its own loopback makes
	/// the check end-to-end in both direct and relay mode — a controller-side TCP
	/// connect would, in relay mode, only reach the always-accepting local tunnel
	/// listener and so couldn't tell a live server from a closed port.
	ProbePort {
		port: u16,
	},
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
	Ssh(SshInfo),
	Logs(LogsResult),
	PortReachable(PortProbe),
	Error { message: String },
}

/// How to reach the agent's built-in SSH server (see [`ControlRequest::EnableSsh`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshInfo {
	/// Loopback port the agent's embedded SSH server listens on. The controller
	/// reaches it through a tunnel (the server binds 127.0.0.1 only).
	pub port: u16,
	/// Username to connect as. The embedded server authenticates by key, not by OS
	/// account, but the client's shell runs as the user the agent runs as.
	pub username: String,
	/// OpenSSH-format ephemeral private key the controller must use (`ssh -i`) to
	/// authenticate. Regenerated per agent run; valid only for this server instance.
	pub private_key: String,
}

/// Result of a [`ControlRequest::ProbePort`]: whether the loopback port had
/// something listening. A struct (not a bare `bool`) because the internally-tagged
/// `ControlResponse` can only carry map-like variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortProbe {
	pub reachable: bool,
}

/// Severity of a single log line, shared by the agent's log buffer and the
/// controller's so the UI can filter both by the same levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
	Error,
	Warn,
	Info,
	Debug,
	Trace,
}

/// One buffered log line. `ts_secs` is Unix seconds when it was recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
	pub ts_secs: u64,
	pub level: LogLevel,
	pub message: String,
}

/// A sender's recent log lines, oldest first — produced by both the agent (its own
/// log) and the relay (its server log). `dropped` is true when the ring buffer
/// evicted older lines before this snapshot, so the UI can flag that the history is
/// partial.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsResult {
	pub lines: Vec<LogLine>,
	pub dropped: bool,
	/// The sender's wall clock (Unix seconds) when it took this snapshot. The
	/// controller re-anchors every line's `ts_secs` to its own clock using
	/// `controller_now - now_secs`, so an agent or relay in another timezone or with
	/// a skewed clock still renders at the correct local time alongside other logs.
	pub now_secs: u64,
	/// Monotonic count of all lines the sender has *ever* recorded (not just those
	/// retained). A consumer polling incrementally passes the previous `next_seq` back
	/// as the next request's cursor to receive only lines recorded since; it also
	/// detects a sender restart (when `next_seq` drops below the cursor it holds).
	pub next_seq: u64,
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

/// Quality/format knobs for a live screen-control session. Can be set at
/// [`SessionClient::Start`] and changed live with [`SessionClient::Configure`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SessionConfig {
	pub display: u32,
	/// Target H.264 bitrate in kilobits per second. The encoder's rate control
	/// sizes each frame to hold the stream around this average; raise it for
	/// sharper motion, lower it for a thinner link.
	pub bitrate_kbps: u32,
	/// Upper bound on frames per second the agent should emit.
	pub max_fps: u8,
	/// Resolution scale as a percentage, 10–100. The agent downscales each
	/// capture by this factor before encoding, trading sharpness for a smaller,
	/// cheaper-to-encode frame. 100 = native resolution.
	pub scale: u8,
	/// Adaptive mode. When set, the agent treats `scale` as a ceiling and lowers
	/// the effective scale automatically when it can't keep up (slow encode or a
	/// congested link), restoring it as conditions clear — "reduce quality to keep
	/// it smooth" without the controller babysitting it.
	pub auto: bool,
}

/// Bitrate clamp bounds (kbps): floor keeps a usable picture, ceiling caps a
/// hostile or fat-fingered config from demanding an absurd allocation/link.
pub const MIN_BITRATE_KBPS: u32 = 200;
pub const MAX_BITRATE_KBPS: u32 = 80_000;

impl SessionConfig {
	/// Clamp every knob into its valid range. Applied on the agent before use so a
	/// malformed or hostile config can't drive a divide-by-zero or a wild allocation.
	pub fn sanitized(self) -> Self {
		Self {
			display: self.display,
			bitrate_kbps: self.bitrate_kbps.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS),
			max_fps: self.max_fps.clamp(1, 60),
			scale: self.scale.clamp(10, 100),
			auto: self.auto,
		}
	}
}

impl Default for SessionConfig {
	fn default() -> Self {
		Self {
			display: 0,
			bitrate_kbps: 8_000,
			max_fps: 30,
			scale: 100,
			auto: false,
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
	/// Change quality/fps/scale on a running session. The agent applies it on the
	/// next frame and emits a fresh keyframe so the new settings take effect cleanly.
	Configure(SessionConfig),
	/// Ask for a fresh full keyframe (e.g. after the UI resized).
	Refresh,
	Stop,
}

/// Agent → controller control messages on the session stream. The high-rate
/// video frames travel as binary [`video`] frames alongside these, not as JSON
/// (see [`video::read_inbound`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum SessionServer {
	/// Sent once at start and whenever the captured (source) geometry changes.
	Meta {
		display: u32,
		width: u32,
		height: u32,
	},
	Error {
		message: String,
	},
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
			ControlRequest::EnableSsh,
			ControlRequest::FetchLogs { max_lines: Some(500) },
			ControlRequest::ProbePort { port: 22 },
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

		// FetchLogs / Logs carry the shared LogLine shape; the level tag and order
		// must survive the round-trip.
		let logs = ControlResponse::Logs(LogsResult {
			lines: vec![
				LogLine {
					ts_secs: 100,
					level: LogLevel::Info,
					message: "starting".into(),
				},
				LogLine {
					ts_secs: 101,
					level: LogLevel::Warn,
					message: "tunnel to 127.0.0.1:22 failed".into(),
				},
			],
			dropped: true,
			now_secs: 200,
			next_seq: 2,
		});
		let back: ControlResponse = round_trip(&logs);
		let ControlResponse::Logs(r) = back else {
			panic!("expected Logs");
		};
		assert!(r.dropped);
		assert_eq!(r.lines.len(), 2);
		assert_eq!(r.lines[1].level, LogLevel::Warn);
		assert_eq!(r.lines[1].message, "tunnel to 127.0.0.1:22 failed");

		// The probe response carries a reachability flag.
		let probe = ControlResponse::PortReachable(PortProbe { reachable: true });
		assert!(matches!(
			round_trip(&probe),
			ControlResponse::PortReachable(PortProbe { reachable: true })
		));
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
