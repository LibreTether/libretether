import { invoke } from "@tauri-apps/api/core"
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
	Frame,
	InputEvent,
	ScreenshotResult,
	SessionMeta,
	Settings
} from "./types"

// ---------------------------------------------------------------- registry
export const listClients = () => invoke<ClientDto[]>("list_clients")
export const createClient = (name: string, os: ClientOs) => invoke<CreateClientResult>("create_client", { name, os })
export const removeClient = (id: string) => invoke<void>("remove_client", { id })
export const renameClient = (id: string, name: string) => invoke<void>("rename_client", { id, name })
export const getDeployScript = (id: string, os?: ClientOs) => invoke<string>("get_deploy_script", { id, os })
export const resetToken = (id: string) => invoke<CreateClientResult>("reset_token", { id })

// ---------------------------------------------------------------- live control
export const clientStatus = (id: string) => invoke<AgentStatus>("client_status", { id })
export const clientExec = (id: string, program: string, args: string[], timeoutSecs?: number) =>
	invoke<ExecResult>("client_exec", { args, id, program, timeoutSecs })
export const clientScreenshot = (id: string, display?: number) =>
	invoke<ScreenshotResult>("client_screenshot", { display, id })

// ---------------------------------------------------------------- session
export interface SessionOpts {
	display?: number
	quality?: number
	maxFps?: number
}
export const startControl = (id: string, opts: SessionOpts = {}) =>
	invoke<void>("start_control", { display: opts.display, id, maxFps: opts.maxFps, quality: opts.quality })
export const sendInput = (id: string, event: InputEvent) => invoke<void>("send_input", { event, id })
export const stopControl = (id: string) => invoke<void>("stop_control", { id })
export const connectRdp = (id: string) => invoke<void>("connect_rdp", { id })
export const connectSsh = (id: string) => invoke<void>("connect_ssh", { id })

// ---------------------------------------------------------------- controllers
export const listControllers = () => invoke<ControllerSummary[]>("list_controllers")
export const createController = (name: string, kind: ControllerKind) =>
	invoke<ControllerSummary>("create_controller", { kind, name })
export const updateController = (id: string, name: string, kind: ControllerKind) =>
	invoke<ControllerSummary>("update_controller", { id, kind, name })
export const deleteController = (id: string) => invoke<void>("delete_controller", { id })
export const selectController = (id: string) => invoke<ActiveInfo>("select_controller", { id })
export const exitController = () => invoke<void>("exit_controller")
export const activeController = () => invoke<ActiveInfo | null>("active_controller")

// ---------------------------------------------------------------- settings
export const getSettings = () => invoke<Settings>("get_settings")
export const setSettings = (rdpClient: string | null, terminal: string | null) =>
	invoke<void>("set_settings", { rdpClient, terminal })
export const saveTextFile = (path: string, contents: string) => invoke<void>("save_text_file", { contents, path })

// ---------------------------------------------------------------- events
export const onClientsChanged = (cb: () => void): Promise<UnlistenFn> => listen("clients:changed", () => cb())
export const onControllerLog = (cb: (line: string) => void): Promise<UnlistenFn> =>
	listen<string>("controller:log", (e) => cb(e.payload))
export const onControllerConnected = (cb: () => void): Promise<UnlistenFn> => listen("controller:connected", () => cb())
export const onSessionFrame = (id: string, cb: (f: Frame) => void): Promise<UnlistenFn> =>
	listen<Frame>(`session:frame:${id}`, (e) => cb(e.payload))
export const onSessionMeta = (id: string, cb: (m: SessionMeta) => void): Promise<UnlistenFn> =>
	listen<SessionMeta>(`session:meta:${id}`, (e) => cb(e.payload))
export const onSessionClosed = (id: string, cb: () => void): Promise<UnlistenFn> =>
	listen(`session:closed:${id}`, () => cb())
export const onSessionError = (id: string, cb: (msg: string) => void): Promise<UnlistenFn> =>
	listen<string>(`session:error:${id}`, (e) => cb(e.payload))

/** Normalise an error thrown from `invoke` into a readable string. Tauri can
 *  reject with a plain object (e.g. `{ message }`) rather than a string/Error, so
 *  pull a string `message` out of those instead of yielding "[object Object]". */
export function errString(e: unknown): string {
	if (typeof e === "string") return e
	if (e instanceof Error) return e.message
	if (e && typeof e === "object" && "message" in e && typeof e.message === "string") return e.message
	return String(e)
}
