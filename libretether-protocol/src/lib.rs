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
pub mod tls;

use serde::{Deserialize, Serialize};

/// ALPN protocol identifier negotiated during the QUIC/TLS handshake.
pub const ALPN: &[u8] = b"libretether/1";

/// Bumped whenever the wire format changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default UDP port the controller listens on for incoming agents.
pub const DEFAULT_PORT: u16 = 47600;

// ---------------------------------------------------------------- handshake

/// Basic identification of the machine an agent runs on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
	pub hostname: String,
	pub os: String,
	pub arch: String,
	pub username: String,
}

/// Server → agent, first message on the handshake stream: a nonce to sign.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Challenge {
	pub nonce: String,
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
	pub host: HostInfo,
	pub agent_version: String,
}

/// Server → agent: the verdict of the handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloAck {
	pub accepted: bool,
	pub reason: Option<String>,
	/// The controller-assigned client id, echoed back so the agent can log it.
	pub client_id: Option<String>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
