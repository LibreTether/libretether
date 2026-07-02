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
	/** The encoders this agent can actually run (never includes "auto"). The Configure
	 *  UI grays out anything not listed. */
	encoders: EncoderPref[]
}

/** Which H.264 encoder the agent should use. Mirrors the Rust `EncoderPref` (serde
 *  snake_case). `auto` = let the agent pick; the others are explicit. */
export type EncoderPref = "auto" | "software" | "hardware" | "gpu"

export interface ClientDto {
	id: string
	name: string
	os: ClientOs
	created_at: number
	enrolled: boolean
	online: boolean
	last_seen: number | null
	status: AgentStatus | null
	/** The agent's Ed25519 public key (base64), set at enrollment — its stable
	 *  identity, and what every connection's signature is checked against. `null`
	 *  until the machine enrolls. */
	public_key: string | null
	/** The encoder this controller has configured this machine to use (persisted on
	 *  the controller, sent to the agent at session start). */
	encoder: EncoderPref
}

export interface CreateClientResult {
	client: ClientDto
	deploy_script: string
}

/** Result of starting a phone-pairing (`open_pairing`): the pending client, the
 *  short code to read aloud, and the portal URL the new machine opens. The pairing
 *  then completes in the background — listen for `onPairingCompleted`. */
export interface PairingStarted {
	client: ClientDto
	code: string
	portal_url: string
}

/** Payload of the `pairing:completed` event. `code` identifies which pairing it
 *  refers to; `phrase` is the verify phrase on success, `error` the reason on
 *  failure. */
export interface PairingCompleted {
	ok: boolean
	code: string
	phrase?: string
	error?: string
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

/** A tenant's freshly-minted credentials, returned by `provisionRelayTenant`.
 *  Field names are snake_case to match the Rust `TenantCredentials` serde shape. */
export interface TenantCredentials {
	tenant_id: string
	name: string
	owner_secret: string
	agent_secret: string
}

/** A relay tenant's public status (no secrets), from `listRelayTenants`. */
export interface TenantInfo {
	tenant_id: string
	name: string
	controller_online: boolean
	agents_online: number
}

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
	/** The controller's full Ed25519 public key (base64) — the `controller_key`
	 *  every agent pins and checks the controller's signature against. */
	public_key: string
	/** The wire protocol version this controller speaks; controller and agents
	 *  must match exactly. */
	protocol_version: number
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
	/** Guest capture backend, e.g. "DXGI", "GDI", "xcap", "PipeWire". */
	capture: string
	/** Guest video encoder, e.g. "OpenH264 (software)", "Media Foundation (hardware)". */
	encoder: string
}

/** Mirrors the Rust `SessionConfig` — the live screen-control quality knobs.
 *  Sent to `start_control` and `configure_control`. */
export interface SessionConfig {
	display: number
	/** Target H.264 bitrate in kbps (clamped 200–80000 on the agent). */
	bitrate_kbps: number
	/** Upper bound on frames per second. */
	max_fps: number
	/** Resolution scale percentage, 10–100 (100 = native). */
	scale: number
	/** Adaptive mode: the agent lowers `scale` automatically when it can't keep up. */
	auto: boolean
	/** The encoder the agent should use. Optional here — the backend injects the
	 *  machine's persisted choice at `start_control`, so the UI doesn't set it. */
	encoder?: EncoderPref
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

// ---------------------------------------------------------------- file transfer

/** Mirrors the Rust `EntryKind` (serde snake_case). */
export type EntryKind = "file" | "dir" | "symlink" | "other"

/** One entry in a directory listing. Mirrors the Rust `DirEntry`. */
export interface DirEntry {
	name: string
	kind: EntryKind
	/** Size in bytes (0 for directories). */
	size: number
	/** Modification time in Unix seconds, when available. */
	mtime: number | null
}

/** A browsed directory. Mirrors the Rust `DirListing`. `roots` is populated only on a
 *  seed request (path === null) — the home dir + drives/`/` to jump to. */
export interface DirListing {
	path: string
	parent: string | null
	roots: string[]
	entries: DirEntry[]
}

/** Direction of a transfer relative to the agent. Mirrors the Rust `Direction`. */
export type TransferDirection = "download" | "upload"

/** Lifecycle of a queued transfer. Mirrors the Rust `TransferStatus`. */
export type TransferStatus = "queued" | "active" | "paused" | "done" | "error"

/** A queued/running/finished transfer. Mirrors the Rust `TransferItem`. `remote_path`
 *  is the source (download) or destination dir (upload) on the agent; `local_path` is
 *  the destination dir (download) or source (upload) on this host. */
export interface TransferItem {
	id: string
	client_id: string
	direction: TransferDirection
	remote_path: string
	local_path: string
	is_dir: boolean
	name: string
	total_files: number
	total_bytes: number
	files_done: number
	bytes_done: number
	status: TransferStatus
	error: string | null
	created_at: number
	updated_at: number
}

/** Payload of the global `transfer:progress` event. */
export interface TransferProgress {
	id: string
	files_done: number
	bytes_done: number
	total_files: number
	total_bytes: number
}
