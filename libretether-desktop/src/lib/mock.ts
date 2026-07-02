// Browser-preview mock. Loaded only by `api.ts` when the Tauri runtime is absent
// (i.e. `run desktop:dev:web`), so the UI can be designed and screenshotted in a
// plain browser. Never reached inside the packaged app — Tauri always defines
// `__TAURI_INTERNALS__`, so `api.ts` calls the real `invoke` there and this module
// stays a lazily-loaded dev chunk.

import type { UnlistenFn } from "@tauri-apps/api/event"
import type {
	ActiveInfo,
	AgentStatus,
	ClientDto,
	ControllerSummary,
	CreateClientResult,
	DirListing,
	ExecResult,
	LogEntry,
	PairingStarted,
	ScreenshotResult,
	Settings,
	TenantCredentials,
	TenantInfo,
	TransferItem
} from "./types"

const NOW = Math.floor(Date.now() / 1000)

// The code from the most recent mock `open_pairing`, so the faked
// `pairing:completed` event can echo it back (the UI matches on it).
let lastPairingCode = ""

function status(over: Partial<AgentStatus["host"]> & Partial<AgentStatus>): AgentStatus {
	return {
		agent_version: "0.17.0",
		arch: over.arch ?? "x86_64",
		boot_time_secs: null,
		displays: over.displays ?? 1,
		encoders: over.encoders ?? ["software"],
		host: {
			arch: over.arch ?? "x86_64",
			hostname: over.hostname ?? "host",
			os: over.os ?? "Linux",
			username: over.username ?? "user"
		},
		started_at: NOW - (over.uptime_secs ?? 0),
		tailscale_ip: over.tailscale_ip ?? null,
		uptime_secs: over.uptime_secs ?? 0
	} as AgentStatus
}

const CLIENTS: ClientDto[] = [
	{
		created_at: NOW - 40 * 86400,
		encoder: "auto",
		enrolled: true,
		id: "m1",
		last_seen: NOW - 4,
		name: "office-imac",
		online: true,
		os: "macos",
		public_key: "Lq3mZ8t1Rv0KpN7sXyB2dWfE5hUcA9gJ6oQ4nT0bIk=",
		status: status({
			arch: "arm64",
			displays: 2,
			hostname: "office-imac.local",
			os: "macOS 14.5",
			tailscale_ip: "100.74.10.2",
			uptime_secs: 3 * 86400 + 4 * 3600,
			username: "vero"
		})
	},
	{
		created_at: NOW - 9 * 86400,
		encoder: "auto",
		enrolled: true,
		id: "m2",
		last_seen: NOW - 2,
		name: "build-box",
		online: true,
		os: "linux",
		public_key: "9fK2pXwQ7mLcV4nB1tR8sZ0aY6eH3jD5uG7oI2kN0xM=",
		status: status({
			displays: 1,
			hostname: "buildbox",
			os: "Ubuntu 24.04",
			tailscale_ip: "100.74.10.5",
			uptime_secs: 12 * 60,
			username: "ci"
		})
	},
	{
		created_at: NOW - 5 * 86400,
		encoder: "auto",
		enrolled: true,
		id: "m3",
		last_seen: NOW - 2,
		name: "win-laptop",
		online: true,
		os: "windows",
		public_key: "Pz5rT8wQ2mC4vB7nX1kL9sA0dY6eH3jF5uG8oI2bN4xW=",
		status: status({
			displays: 3,
			encoders: ["software", "hardware", "gpu"],
			hostname: "DESKTOP-7F2K",
			os: "Windows 11 Pro",
			tailscale_ip: "100.74.10.9",
			uptime_secs: 5 * 3600 + 22 * 60,
			username: "vero"
		})
	},
	{
		created_at: NOW - 60 * 86400,
		encoder: "auto",
		enrolled: true,
		id: "m4",
		last_seen: NOW - 3 * 3600,
		name: "nas",
		online: false,
		os: "linux",
		public_key: "Aa1bB2cC3dD4eE5fF6gG7hH8iI9jJ0kK1lL2mM3nN4o=",
		status: null
	},
	{
		created_at: NOW - 200,
		encoder: "auto",
		enrolled: false,
		id: "m5",
		last_seen: null,
		name: "new-mini",
		online: false,
		os: "linux",
		public_key: null,
		status: null
	}
]

const CONTROLLERS: ControllerSummary[] = [
	{
		active: true,
		fingerprint: "a1b2 c3d4 e5f6 7890",
		id: "c1",
		kind: { auth_key: "tskey-auth-demo", listen_port: 47600, type: "tailscale" },
		machine_count: 5,
		name: "Home lab"
	},
	{
		active: false,
		fingerprint: "99aa bb22 cc33 dd44",
		id: "c2",
		kind: {
			address: "relay.example.com:47600",
			agent_secret: "agent-9f2c7b41e0a8",
			owner_secret: "owner-7c3a1d92f5b6",
			type: "relay"
		},
		machine_count: 2,
		name: "Cloud relay"
	}
]

const ACTIVE: ActiveInfo = {
	fingerprint: "Ctrl0KeyZ9x8",
	id: "c1",
	kind: { auth_key: "tskey-auth-demo", listen_port: 47600, type: "tailscale" },
	name: "Home lab",
	protocol_version: 5,
	public_key: "Ctrl0KeyZ9x8W7v6U5t4S3r2Q1p0O9n8M7l6K5j4H3g=",
	reachable_at: "100.74.10.1:47600",
	tailscale: { address: "100.74.10.1", hostname: "home-lab", installed: true, running: true }
}

const LOGS: LogEntry[] = [
	{
		level: "info",
		message: "controller listening on 100.74.10.1:47600 (quic)",
		source: "controller",
		ts_secs: NOW - 320
	},
	{
		level: "info",
		message: "agent office-imac authenticated (ed25519 ok)",
		source: "controller",
		ts_secs: NOW - 300
	},
	{ level: "debug", message: "capability token issued for session 4f2a", source: "tunnel", ts_secs: NOW - 280 },
	{ level: "info", message: "agent build-box connected", source: "controller", ts_secs: NOW - 240 },
	{
		level: "warn",
		message: "agent nas missed 3 keepalives — marking offline",
		source: "controller",
		ts_secs: NOW - 180
	},
	{
		level: "info",
		message: "screen session started for win-laptop (1920x1080)",
		source: "controller",
		ts_secs: NOW - 60
	},
	{ level: "error", message: "rdp probe to nas timed out after 5s", source: "tunnel", ts_secs: NOW - 30 }
]

const SCREENSHOT_PNG =
	"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="

const settings: Settings = { rdp_client: null, terminal: null }

function deployScript(name: string): string {
	return `curl -fsSL https://github.com/LibreTether/libretether/releases/latest/download/install-linux.sh | sh -s -- --token tok_${name}_9f2c --controller 100.74.10.1:47600 --controller-key a1b2c3d4e5f6 --tailscale-key tskey-auth-demo`
}

const delay = <T>(value: T): Promise<T> => new Promise((r) => setTimeout(() => r(value), 130))

// A tiny fake filesystem for the transfer browser preview.
function mockDir(path: string | null): DirListing {
	const base = path ?? "/home/user"
	return {
		entries: [
			{ kind: "dir", mtime: NOW - 3600, name: "Documents", size: 0 },
			{ kind: "dir", mtime: NOW - 7200, name: "Downloads", size: 0 },
			{ kind: "file", mtime: NOW - 120, name: "notes.txt", size: 2048 },
			{ kind: "file", mtime: NOW - 86400, name: "archive.tar.gz", size: 48 * 1024 * 1024 },
			{ kind: "symlink", mtime: NOW - 500, name: "latest", size: 12 }
		],
		parent: base === "/" ? null : "/",
		path: base,
		roots: path === null ? ["/home/user", "/"] : []
	}
}

const TRANSFERS: TransferItem[] = [
	{
		bytes_done: 48 * 1024 * 1024,
		client_id: "m2",
		created_at: NOW - 30,
		direction: "download",
		error: null,
		files_done: 3,
		id: "t1",
		is_dir: true,
		local_path: "/home/user/Downloads",
		name: "build-artifacts",
		remote_path: "/var/out/build-artifacts",
		status: "active",
		total_bytes: 120 * 1024 * 1024,
		total_files: 8,
		updated_at: NOW
	}
]

export function mockInvoke(cmd: string, args?: Record<string, unknown>): Promise<unknown> {
	const id = args?.id as string | undefined
	const find = (cid?: string) => CLIENTS.find((c) => c.id === cid)
	switch (cmd) {
		case "list_clients":
			return delay(CLIENTS)
		case "active_controller":
			return delay(ACTIVE)
		case "list_controllers":
			return delay(CONTROLLERS)
		case "select_controller":
			return delay(ACTIVE)
		case "create_controller":
		case "update_controller":
			return delay(CONTROLLERS[0])
		case "client_status":
			return delay(find(id)?.status ?? status({ hostname: "host" }))
		case "get_settings":
			return delay(settings)
		case "get_deploy_script":
			return delay(deployScript((args?.id as string) ?? "demo"))
		case "create_client": {
			const name = (args?.name as string) ?? "machine"
			const result: CreateClientResult = {
				client: {
					created_at: NOW,
					encoder: "auto",
					enrolled: false,
					id: `new-${name}`,
					last_seen: null,
					name,
					online: false,
					os: (args?.os as ClientDto["os"]) ?? "linux",
					public_key: null,
					status: null
				},
				deploy_script: deployScript(name)
			}
			return delay(result)
		}
		case "reset_token": {
			const c = find(id) ?? CLIENTS[0]
			return delay({ client: c, deploy_script: deployScript(c.name) } satisfies CreateClientResult)
		}
		case "open_pairing": {
			const name = (args?.name as string) ?? "machine"
			lastPairingCode = "4F9K-2A7C"
			return delay({
				client: {
					created_at: NOW,
					encoder: "auto",
					enrolled: false,
					id: `pair-${name}`,
					last_seen: null,
					name,
					online: false,
					os: (args?.os as ClientDto["os"]) ?? "linux",
					public_key: null,
					status: null
				},
				code: lastPairingCode,
				portal_url: "https://relay.example.com"
			} satisfies PairingStarted)
		}
		case "client_exec":
			return delay({
				code: 0,
				duration_ms: 42,
				stderr: "",
				stdout: "Linux buildbox 6.8.0 #1 SMP x86_64 GNU/Linux\n"
			} satisfies ExecResult)
		case "client_screenshot":
			return delay({ display: 0, height: 1, png_base64: SCREENSHOT_PNG, width: 1 } satisfies ScreenshotResult)
		case "set_client_encoder": {
			const c = find(id)
			const next = args?.encoder as ClientDto["encoder"]
			const changed = !!c && c.encoder !== next
			if (c && next) c.encoder = next
			return delay(changed)
		}
		case "get_controller_logs":
			return delay(LOGS)
		case "client_logs":
			return delay([
				{
					level: "info",
					message: "agent up; backend=wayland (portal)",
					source: find(id)?.name ?? "agent",
					ts_secs: NOW - 200
				},
				{
					level: "debug",
					message: "captured frame 12.3ms (jpeg q70)",
					source: find(id)?.name ?? "agent",
					ts_secs: NOW - 90
				}
			] satisfies LogEntry[])
		case "relay_logs":
			return delay([
				{ level: "info", message: "relay listening on udp/0.0.0.0:47600", source: "relay", ts_secs: NOW - 600 },
				{ level: "info", message: "controller connected (a1b2c3d4…)", source: "relay", ts_secs: NOW - 540 },
				{ level: "info", message: "agent connected (9f2c7b41…)", source: "relay", ts_secs: NOW - 120 },
				{
					level: "warn",
					message: "agent connection refused: at agent capacity",
					source: "relay",
					ts_secs: NOW - 40
				}
			] satisfies LogEntry[])
		case "provision_relay_tenant":
			return delay({
				agent_secret: "agent-mock-secret-000000",
				name: (args?.name as string) ?? "tenant",
				owner_secret: "owner-mock-secret-000000",
				tenant_id: "tnt_mock01"
			} satisfies TenantCredentials)
		case "list_relay_tenants":
			return delay([
				{ agents_online: 2, controller_online: true, name: "team-a", tenant_id: "tnt_mock01" },
				{ agents_online: 0, controller_online: false, name: "team-b", tenant_id: "tnt_mock02" }
			] satisfies TenantInfo[])
		case "revoke_relay_tenant":
			return delay(true)
		case "set_settings":
			settings.rdp_client = (args?.rdpClient as string | null) ?? null
			settings.terminal = (args?.terminal as string | null) ?? null
			return delay(undefined)
		case "browse_remote":
		case "browse_local":
			return delay(mockDir((args?.path as string | null) ?? null))
		case "list_transfers":
			return delay(TRANSFERS)
		case "enqueue_transfer":
			return delay({
				bytes_done: 0,
				client_id: (args?.clientId as string) ?? "m1",
				created_at: NOW,
				direction: (args?.direction as TransferItem["direction"]) ?? "download",
				error: null,
				files_done: 0,
				id: `t-${NOW}`,
				is_dir: (args?.isDir as boolean) ?? false,
				local_path: (args?.localPath as string) ?? "/",
				name: "queued-item",
				remote_path: (args?.remotePath as string) ?? "/",
				status: "queued",
				total_bytes: 0,
				total_files: 0,
				updated_at: NOW
			} satisfies TransferItem)
		default:
			// remove_client, rename_client, start/stop control, connect_rdp/ssh,
			// send_input, save_text_file, exit_controller, delete_controller …
			return delay(undefined)
	}
}

export function mockListen(event: string, cb: (e: { payload: unknown }) => void): UnlistenFn {
	// Give the live-control overlay a resolution so it leaves the "starting" state.
	if (event.startsWith("session:meta:")) {
		const t = setTimeout(
			() =>
				cb({
					payload: { capture: "xcap", display: 0, encoder: "OpenH264 (software)", height: 1080, width: 1920 }
				}),
			300
		)
		return () => clearTimeout(t)
	}
	// Fake a successful pairing shortly after the phone-install view subscribes.
	if (event === "pairing:completed") {
		const t = setTimeout(
			() => cb({ payload: { code: lastPairingCode, ok: true, phrase: "tiger-river-otter-maple" } }),
			2500
		)
		return () => clearTimeout(t)
	}
	return () => {}
}
