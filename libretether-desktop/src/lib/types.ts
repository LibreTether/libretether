// These types mirror the structs serialised by the Rust backend. They are
// hand-maintained, but guarded: a Rust-side test (`commands.rs` →
// `*_match_types_ts`) pins the JSON field set and enum tags below, so renaming or
// dropping a field there fails `cargo test` until this file is updated to match.

export type ClientOs = "linux" | "macos" | "windows"

export interface HostInfo {
	hostname: string
	os: string
	arch: string
	username: string
}

export interface AgentStatus {
	host: HostInfo
	agent_version: string
	uptime_secs: number
	started_at: number
	boot_time_secs: number | null
	displays: number
	tailscale_ip: string | null
}

export interface ClientDto {
	id: string
	name: string
	os: ClientOs
	created_at: number
	enrolled: boolean
	online: boolean
	last_seen: number | null
	status: AgentStatus | null
}

export interface CreateClientResult {
	client: ClientDto
	deploy_script: string
}

export interface TailscaleInfo {
	installed: boolean
	running: boolean
	address: string | null
	hostname: string | null
}

// A controller's connection type. Field names are snake_case to match the Rust
// serde enum (tag "type") — the object is sent verbatim to the backend.
export type ControllerKind =
	| { type: "direct"; advertise_addr: string | null; listen_port: number }
	| { type: "tailscale"; auth_key: string | null; listen_port: number }
	| { type: "relay"; address: string; owner_secret: string; agent_secret: string }

export type ControllerType = ControllerKind["type"]

export interface ControllerSummary {
	id: string
	name: string
	kind: ControllerKind
	fingerprint: string
	machine_count: number
	active: boolean
}

export interface ActiveInfo {
	id: string
	name: string
	kind: ControllerKind
	fingerprint: string
	reachable_at: string | null
	tailscale: TailscaleInfo | null
}

export interface Settings {
	rdp_client: string | null
	terminal: string | null
}

/** The RDP-client preference the Connection page picks between. The preset values
 *  match what the backend's `rdp.rs` switches on; "custom" means use `rdp_custom`. */
export type RdpMode = "auto" | "freerdp" | "remmina" | "gnome-connections" | "custom"

export interface ExecResult {
	code: number | null
	stdout: string
	stderr: string
	duration_ms: number
}

export interface ScreenshotResult {
	display: number
	width: number
	height: number
	png_base64: string
}

// ---------------------------------------------------------------- live session

export interface SessionMeta {
	display: number
	width: number
	height: number
}

export type FrameEncoding = "jpeg" | "png"

export interface Frame {
	seq: number
	width: number
	height: number
	encoding: FrameEncoding
	data_base64: string
}

// ---------------------------------------------------------------- logs

/** Mirrors the Rust `LogLevel` enum (serde snake_case). */
export type LogLevel = "error" | "warn" | "info" | "debug" | "trace"

/** A single log line shown on the Logs page. Controller lines arrive live via the
 *  `logs:entry` event; agent lines are fetched on demand. `source` is "controller"
 *  / "tunnel" for the app itself, or the client's name for an agent's log. */
export interface LogEntry {
	ts_secs: number
	level: LogLevel
	source: string
	message: string
}

export type MouseButton = "left" | "right" | "middle"

/** Mirrors the Rust `InputEvent` enum (serde tag "t", snake_case). */
export type InputEvent =
	| { t: "mouse_move"; x: number; y: number }
	| { t: "mouse_button"; button: MouseButton; pressed: boolean }
	| { t: "mouse_scroll"; dx: number; dy: number }
	| { t: "key"; code: string; pressed: boolean }
	| { t: "text"; text: string }
