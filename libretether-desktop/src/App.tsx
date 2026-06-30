import {
	Eye,
	MonitorSmartphone,
	MonitorUp,
	Plug,
	Plus,
	Rocket,
	ScreenShare,
	ScrollText,
	SlidersHorizontal,
	Terminal
} from "lucide-react"
import { useCallback, useEffect, useMemo, useState } from "react"
import { AddMachineDrawer } from "./components/AddMachineDrawer"
import { type Command, CommandPalette } from "./components/CommandPalette"
import { ControlOverlay } from "./components/ControlOverlay"
import { ConfirmProvider } from "./components/confirm"
import { DeployScript } from "./components/DeployScript"
import { DetailDrawer } from "./components/DetailDrawer"
import { ShortcutsOverlay } from "./components/ShortcutsOverlay"
import { type Page, Sidebar } from "./components/Sidebar"
import { Button, Drawer, Spinner } from "./components/ui"
import * as api from "./lib/api"
import { useHotkeys } from "./lib/hotkeys"
import { ToastProvider, useToast } from "./lib/toast"
import type { ActiveInfo, ClientDto } from "./lib/types"
import { type DeployState, useMachineActions } from "./lib/useMachineActions"
import { ConnectionPage } from "./pages/ConnectionPage"
import { ControllerSelect } from "./pages/ControllerSelect"
import { LogsPage } from "./pages/LogsPage"
import { MachinesPage } from "./pages/MachinesPage"

function Shell({ active, onExit }: { active: ActiveInfo; onExit: () => void }) {
	const toast = useToast()
	const [page, setPage] = useState<Page>("machines")
	const [clients, setClients] = useState<ClientDto[]>([])
	const [loading, setLoading] = useState(true)

	// The live-session overlay, or null. `watch` opens it read-only: it receives the
	// stream but forwards no input (see ControlOverlay's `readOnly`).
	const [controlSession, setControlSession] = useState<{ id: string; watch: boolean } | null>(null)
	const [detailId, setDetailId] = useState<string | null>(null)
	const [addOpen, setAddOpen] = useState(false)
	const [deploy, setDeploy] = useState<DeployState | null>(null)
	const [paletteOpen, setPaletteOpen] = useState(false)
	const [shortcutsOpen, setShortcutsOpen] = useState(false)

	const reload = useCallback(() => {
		api.listClients()
			.then(setClients)
			.catch((e) => toast.error("Couldn't load machines", api.errString(e)))
			.finally(() => setLoading(false))
	}, [toast])

	useEffect(() => {
		reload()
		const unlisten = api.onClientsChanged(reload)
		return () => {
			unlisten.then((fn) => fn())
		}
	}, [reload])

	const actions = useMachineActions(reload, setDeploy)

	// Derive the open overlays from live `clients` so they stay in sync (and close
	// themselves if a machine is removed underneath them).
	const control = controlSession ? (clients.find((c) => c.id === controlSession.id) ?? null) : null
	const detail = detailId ? (clients.find((c) => c.id === detailId) ?? null) : null
	useEffect(() => {
		if (controlSession && !clients.some((c) => c.id === controlSession.id)) setControlSession(null)
		if (detailId && !clients.some((c) => c.id === detailId)) setDetailId(null)
	}, [clients, controlSession, detailId])

	const exit = async () => {
		try {
			await api.exitController()
		} catch {
			/* exiting regardless */
		}
		onExit()
	}

	const controlling = !!control
	const overlayOpen = paletteOpen || shortcutsOpen || addOpen || !!detail || !!deploy

	// Command palette + help are reachable any time the live-control surface isn't
	// capturing the keyboard.
	useHotkeys(
		[
			{ allowInInput: true, combo: "mod+k", handler: () => setPaletteOpen((o) => !o) },
			{ combo: "?", handler: () => setShortcutsOpen(true) }
		],
		!controlling
	)

	// Page navigation + add only fire when nothing is layered on top.
	useHotkeys(
		[
			{ combo: "1", handler: () => setPage("machines") },
			{ combo: "2", handler: () => setPage("controller") },
			{ combo: "3", handler: () => setPage("logs") },
			{
				combo: "n",
				handler: () => {
					setPage("machines")
					setAddOpen(true)
				}
			}
		],
		!controlling && !overlayOpen
	)

	const commands = useMemo<Command[]>(() => {
		const list: Command[] = [
			{
				group: "Navigate",
				hint: "1",
				icon: <MonitorSmartphone className="h-4 w-4" />,
				id: "nav-machines",
				label: "Machines",
				run: () => setPage("machines")
			},
			{
				group: "Navigate",
				hint: "2",
				icon: <Plug className="h-4 w-4" />,
				id: "nav-conn",
				label: "Connection",
				run: () => setPage("controller")
			},
			{
				group: "Navigate",
				hint: "3",
				icon: <ScrollText className="h-4 w-4" />,
				id: "nav-logs",
				label: "Logs",
				run: () => setPage("logs")
			},
			{
				group: "Quick actions",
				hint: "N",
				icon: <Plus className="h-4 w-4" />,
				id: "act-add",
				label: "Add a machine",
				run: () => {
					setPage("machines")
					setAddOpen(true)
				}
			}
		]
		for (const c of clients) {
			const kw = `${c.name} ${c.status?.host.hostname ?? ""}`
			list.push(
				{
					disabled: !c.online,
					group: c.name,
					icon: <MonitorUp className="h-4 w-4" />,
					id: `ctl:${c.id}`,
					keywords: `control screen takeover ${kw}`,
					label: "Take control",
					run: () => setControlSession({ id: c.id, watch: false })
				},
				{
					disabled: !c.online,
					group: c.name,
					icon: <Eye className="h-4 w-4" />,
					id: `watch:${c.id}`,
					keywords: `watch view observe read-only screen ${kw}`,
					label: "Watch (read-only)",
					run: () => setControlSession({ id: c.id, watch: true })
				},
				{
					disabled: !c.online,
					group: c.name,
					icon: <Terminal className="h-4 w-4" />,
					id: `ssh:${c.id}`,
					keywords: `ssh terminal shell ${kw}`,
					label: "Connect via SSH",
					run: () => actions.ssh(c)
				},
				{
					disabled: !c.online,
					group: c.name,
					icon: <ScreenShare className="h-4 w-4" />,
					id: `rdp:${c.id}`,
					keywords: `rdp remote desktop ${kw}`,
					label: "Connect via RDP",
					run: () => actions.rdp(c)
				},
				{
					disabled: !c.online,
					group: c.name,
					icon: <SlidersHorizontal className="h-4 w-4" />,
					id: `det:${c.id}`,
					keywords: `details status command screenshot ${kw}`,
					label: "Open details",
					run: () => setDetailId(c.id)
				},
				{
					group: c.name,
					icon: <Rocket className="h-4 w-4" />,
					id: `dep:${c.id}`,
					keywords: `deploy install script enroll ${kw}`,
					label: c.enrolled ? "Re-deploy…" : "Get deploy command",
					run: () => actions.deploy(c)
				}
			)
		}
		return list
	}, [clients, actions])

	return (
		<div className="flex h-screen overflow-hidden">
			<Sidebar
				active={active}
				onExit={exit}
				onNavigate={setPage}
				onOpenPalette={() => setPaletteOpen(true)}
				onOpenShortcuts={() => setShortcutsOpen(true)}
				page={page}
			/>
			<main className="flex min-w-0 flex-1 flex-col overflow-hidden">
				{page === "machines" && (
					<MachinesPage
						actions={actions}
						clients={clients}
						hotkeysEnabled={!controlling && !overlayOpen}
						loading={loading}
						onAdd={() => setAddOpen(true)}
						onControl={(c) => setControlSession({ id: c.id, watch: false })}
						onDetail={(c) => setDetailId(c.id)}
						onWatch={(c) => setControlSession({ id: c.id, watch: true })}
					/>
				)}
				{page === "controller" && <ConnectionPage active={active} />}
				{page === "logs" && <LogsPage clients={clients} hotkeysEnabled={!controlling && !overlayOpen} />}
			</main>

			<CommandPalette commands={commands} onClose={() => setPaletteOpen(false)} open={paletteOpen} />
			<ShortcutsOverlay onClose={() => setShortcutsOpen(false)} open={shortcutsOpen} />
			<AddMachineDrawer onClose={() => setAddOpen(false)} onCreated={reload} open={addOpen} />
			{detail && (
				<DetailDrawer
					client={detail}
					key={detail.id}
					onClose={() => setDetailId(null)}
					open
					showTailscale={active.kind.type === "tailscale"}
				/>
			)}
			{deploy && (
				<Drawer
					footer={
						<Button onClick={() => setDeploy(null)} variant="outline">
							Done
						</Button>
					}
					icon={<Rocket className="h-5 w-5" />}
					onClose={() => setDeploy(null)}
					open
					size="md"
					subtitle="Run the command on the target machine"
					title={`Deploy ${deploy.name}`}
				>
					<DeployScript name={deploy.name} os={deploy.os} script={deploy.script} />
				</Drawer>
			)}
			{control && (
				<ControlOverlay
					client={control}
					onClose={() => setControlSession(null)}
					readOnly={controlSession?.watch}
				/>
			)}
		</div>
	)
}

function Root() {
	const [active, setActive] = useState<ActiveInfo | null>(null)
	const [loading, setLoading] = useState(true)

	useEffect(() => {
		api.activeController()
			.then(setActive)
			.catch(() => setActive(null))
			.finally(() => setLoading(false))
	}, [])

	if (loading) {
		return (
			<div className="grid h-screen place-items-center">
				<Spinner className="h-6 w-6" />
			</div>
		)
	}

	return active ? (
		<Shell active={active} onExit={() => setActive(null)} />
	) : (
		<ControllerSelect onConnected={setActive} />
	)
}

export default function App() {
	return (
		<ToastProvider>
			<ConfirmProvider>
				<Root />
			</ConfirmProvider>
		</ToastProvider>
	)
}
