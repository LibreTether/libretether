import { Channel, invoke } from "@tauri-apps/api/core"
import { listen, type UnlistenFn } from "@tauri-apps/api/event"
import type {
	ActiveInfo,
	AgentStatus,
	ClientDto,
	ClientOs,
	ControllerKind,
	ControllerSummary,
	CreateClientResult,
	ExecResult,
	InputEvent,
	LogEntry,
	PairingCompleted,
	PairingStarted,
	ScreenshotResult,
	SessionConfig,
	SessionMeta,
	Settings
} from "./types"

// The packaged app always runs inside Tauri (which defines `__TAURI_INTERNALS__`)
// and talks to the real backend. When that runtime is absent — `desktop:dev:web`,
// a plain browser for UI design — calls fall back to a lazily-loaded mock so the
// interface renders with representative data. The mock chunk never loads in prod.
const HAS_TAURI = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window

function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
	if (HAS_TAURI) return invoke<T>(cmd, args)
	return import("./mock").then((m) => m.mockInvoke(cmd, args) as T)
}

function sub<T>(event: string, cb: (e: { payload: T }) => void): Promise<UnlistenFn> {
	if (HAS_TAURI) return listen<T>(event, cb)
	return import("./mock").then((m) => m.mockListen(event, cb as (e: { payload: unknown }) => void))
}

// ---------------------------------------------------------------- registry
export const listClients = () => call<ClientDto[]>("list_clients")
export const createClient = (name: string, os: ClientOs) => call<CreateClientResult>("create_client", { name, os })
export const removeClient = (id: string) => call<void>("remove_client", { id })
export const renameClient = (id: string, name: string) => call<void>("rename_client", { id, name })
export const getDeployScript = (id: string, os?: ClientOs) => call<string>("get_deploy_script", { id, os })
export const resetToken = (id: string) => call<CreateClientResult>("reset_token", { id })
export const openPairing = (name: string, os: ClientOs) => call<PairingStarted>("open_pairing", { name, os })

// ---------------------------------------------------------------- live control
export const clientStatus = (id: string) => call<AgentStatus>("client_status", { id })
export const clientExec = (id: string, program: string, args: string[], timeoutSecs?: number) =>
	call<ExecResult>("client_exec", { args, id, program, timeoutSecs })
export const clientScreenshot = (id: string, display?: number) =>
	call<ScreenshotResult>("client_screenshot", { display, id })

// ---------------------------------------------------------------- session
/** Start a live screen-control session. Binary video frames arrive on `onFrame`
 *  as `ArrayBuffer`s (one per encoded frame) over a Tauri channel — no base64. */
export const startControl = (
	id: string,
	config: SessionConfig,
	onFrame: (frame: ArrayBuffer) => void
): Promise<void> => {
	if (!HAS_TAURI) return import("./mock").then((m) => m.mockInvoke("start_control", { config, id }) as Promise<void>)
	const frames = new Channel<ArrayBuffer>()
	frames.onmessage = onFrame
	return invoke<void>("start_control", { config, frames, id })
}
/** Change the live session's quality/fps/scale without restarting it. */
export const configureControl = (id: string, config: SessionConfig) => call<void>("configure_control", { config, id })
export const sendInput = (id: string, event: InputEvent) => call<void>("send_input", { event, id })
export const stopControl = (id: string) => call<void>("stop_control", { id })
export const connectRdp = (id: string) => call<void>("connect_rdp", { id })
export const connectSsh = (id: string) => call<void>("connect_ssh", { id })

// ---------------------------------------------------------------- controllers
export const listControllers = () => call<ControllerSummary[]>("list_controllers")
export const createController = (name: string, kind: ControllerKind) =>
	call<ControllerSummary>("create_controller", { kind, name })
export const updateController = (id: string, name: string, kind: ControllerKind) =>
	call<ControllerSummary>("update_controller", { id, kind, name })
export const deleteController = (id: string) => call<void>("delete_controller", { id })
export const selectController = (id: string) => call<ActiveInfo>("select_controller", { id })
export const exitController = () => call<void>("exit_controller")
export const activeController = () => call<ActiveInfo | null>("active_controller")

// ---------------------------------------------------------------- logs
export const getControllerLogs = () => call<LogEntry[]>("get_controller_logs")
export const clientLogs = (id: string, maxLines?: number) => call<LogEntry[]>("client_logs", { id, maxLines })

// ---------------------------------------------------------------- settings
export const getSettings = () => call<Settings>("get_settings")
export const setSettings = (rdpClient: string | null, terminal: string | null) =>
	call<void>("set_settings", { rdpClient, terminal })
export const saveTextFile = (path: string, contents: string) => call<void>("save_text_file", { contents, path })

// ---------------------------------------------------------------- events
export const onClientsChanged = (cb: () => void): Promise<UnlistenFn> => sub("clients:changed", () => cb())
export const onControllerLog = (cb: (line: string) => void): Promise<UnlistenFn> =>
	sub<string>("controller:log", (e) => cb(e.payload))
export const onControllerConnected = (cb: () => void): Promise<UnlistenFn> => sub("controller:connected", () => cb())
export const onLogEntry = (cb: (entry: LogEntry) => void): Promise<UnlistenFn> =>
	sub<LogEntry>("logs:entry", (e) => cb(e.payload))
export const onSessionMeta = (id: string, cb: (m: SessionMeta) => void): Promise<UnlistenFn> =>
	sub<SessionMeta>(`session:meta:${id}`, (e) => cb(e.payload))
export const onSessionClosed = (id: string, cb: () => void): Promise<UnlistenFn> =>
	sub(`session:closed:${id}`, () => cb())
export const onSessionError = (id: string, cb: (msg: string) => void): Promise<UnlistenFn> =>
	sub<string>(`session:error:${id}`, (e) => cb(e.payload))
export const onPairingCompleted = (cb: (e: PairingCompleted) => void): Promise<UnlistenFn> =>
	sub<PairingCompleted>("pairing:completed", (e) => cb(e.payload))

/** Normalise an error thrown from `invoke` into a readable string. Tauri can
 *  reject with a plain object (e.g. `{ message }`) rather than a string/Error, so
 *  pull a string `message` out of those instead of yielding "[object Object]". */
export function errString(e: unknown): string {
	if (typeof e === "string") return e
	if (e instanceof Error) return e.message
	if (e && typeof e === "object" && "message" in e && typeof e.message === "string") return e.message
	return String(e)
}
