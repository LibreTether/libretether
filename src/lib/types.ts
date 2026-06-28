// These types mirror the structs serialised by the Rust backend.

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

export interface ControllerInfo {
	listen_port: number
	fingerprint: string
	tailscale: TailscaleInfo
	advertise_addr: string | null
	tailscale_auth_key: string | null
	rdp_client: string | null
	terminal: string | null
}

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

export type MouseButton = "left" | "right" | "middle"

/** Mirrors the Rust `InputEvent` enum (serde tag "t", snake_case). */
export type InputEvent =
	| { t: "mouse_move"; x: number; y: number }
	| { t: "mouse_button"; button: MouseButton; pressed: boolean }
	| { t: "mouse_scroll"; dx: number; dy: number }
	| { t: "key"; code: string; pressed: boolean }
	| { t: "text"; text: string }
