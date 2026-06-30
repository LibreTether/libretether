import {
	Camera,
	Eye,
	Info,
	MonitorSmartphone,
	MonitorUp,
	Play,
	Plus,
	Rocket,
	ScreenShare,
	Search,
	SquareChevronRight,
	Terminal,
	Trash2,
	X
} from "lucide-react"
import { useEffect, useMemo, useRef, useState } from "react"
import { Menu } from "../components/Menu"
import { OsIcon, osLabel } from "../components/OsIcon"
import { Button, EmptyState, Input, Spinner, StatusDot } from "../components/ui"
import * as api from "../lib/api"
import { cn } from "../lib/cn"
import { formatUptime, relativeTime, tokenizeCommand } from "../lib/format"
import { useHotkeys } from "../lib/hotkeys"
import type { ClientDto, ExecResult } from "../lib/types"
import { useAsyncAction } from "../lib/useAsyncAction"
import type { useMachineActions } from "../lib/useMachineActions"

type Actions = ReturnType<typeof useMachineActions>

function dotState(c: ClientDto): "online" | "offline" | "pending" {
	if (c.online) return "online"
	if (!c.enrolled) return "pending"
	return "offline"
}

export function MachinesPage({
	clients,
	loading,
	onControl,
	onWatch,
	onDetail,
	onAdd,
	actions,
	hotkeysEnabled
}: {
	clients: ClientDto[]
	loading: boolean
	onControl: (c: ClientDto) => void
	onWatch: (c: ClientDto) => void
	onDetail: (c: ClientDto) => void
	onAdd: () => void
	actions: Actions
	hotkeysEnabled: boolean
}) {
	const [query, setQuery] = useState("")
	const [selectedId, setSelectedId] = useState<string | null>(null)
	const searchRef = useRef<HTMLInputElement>(null)
	const listRef = useRef<HTMLDivElement>(null)

	// Re-render every 30s so relative "last seen" labels don't go stale between
	// `clients:changed` events.
	const [, tick] = useState(0)
	useEffect(() => {
		const t = window.setInterval(() => tick((n) => n + 1), 30_000)
		return () => window.clearInterval(t)
	}, [])

	const filtered = useMemo(() => {
		const q = query.trim().toLowerCase()
		if (!q) return clients
		return clients.filter(
			(c) =>
				c.name.toLowerCase().includes(q) ||
				osLabel(c.os).toLowerCase().includes(q) ||
				(c.status?.host.hostname.toLowerCase().includes(q) ?? false)
		)
	}, [clients, query])

	const online = clients.filter((c) => c.online).length
	const awaiting = clients.filter((c) => !c.enrolled).length

	const selected = filtered.find((c) => c.id === selectedId) ?? null

	const move = (delta: number) => {
		if (filtered.length === 0) return
		const i = filtered.findIndex((c) => c.id === selectedId)
		const next =
			i === -1 ? (delta > 0 ? 0 : filtered.length - 1) : Math.min(filtered.length - 1, Math.max(0, i + delta))
		setSelectedId(filtered[next].id)
	}

	// Keep the highlighted row in view.
	useEffect(() => {
		if (selectedId)
			listRef.current
				?.querySelector<HTMLElement>(`[data-id="${selectedId}"]`)
				?.scrollIntoView({ block: "nearest" })
	}, [selectedId])

	useHotkeys(
		[
			{ combo: "/", handler: () => searchRef.current?.focus() },
			{ combo: "arrowdown", handler: () => move(1) },
			{ combo: "j", handler: () => move(1) },
			{ combo: "arrowup", handler: () => move(-1) },
			{ combo: "k", handler: () => move(-1) },
			{ combo: "enter", handler: () => selected?.online && onControl(selected) },
			{ combo: "w", handler: () => selected?.online && onWatch(selected) },
			{ combo: "s", handler: () => selected?.online && actions.ssh(selected) },
			{ combo: "r", handler: () => selected?.online && actions.rdp(selected) },
			{ combo: "d", handler: () => selected?.online && onDetail(selected) }
		],
		hotkeysEnabled
	)

	return (
		<>
			<header className="drag flex flex-wrap items-center justify-between gap-x-4 gap-y-3 border-b border-border bg-surface/30 px-7 py-4">
				<div className="min-w-0">
					<div className="eyebrow mb-1">Fleet</div>
					<h1 className="font-display text-xl font-bold tracking-tight text-text">Machines</h1>
				</div>
				<div className="no-drag flex items-center gap-2">
					<div className="relative">
						<Search className="-translate-y-1/2 pointer-events-none absolute top-1/2 left-3 h-4 w-4 text-subtle" />
						<Input
							className="w-56 pr-8 pl-9"
							onChange={(e) => setQuery(e.target.value)}
							onKeyDown={(e) => {
								if (e.key === "Escape") {
									setQuery("")
									e.currentTarget.blur()
								}
							}}
							placeholder="Search machines…"
							ref={searchRef}
							value={query}
						/>
						{!query && (
							<kbd className="kbd -translate-y-1/2 absolute top-1/2 right-2 h-5 min-w-5 px-1 text-[0.65rem]">
								/
							</kbd>
						)}
					</div>
					<Button icon={<Plus className="h-4 w-4" />} onClick={onAdd} variant="primary">
						Add machine
					</Button>
				</div>
			</header>

			{/* Status strip: the fleet's reachability at a glance. */}
			<div className="flex items-center gap-4 border-b border-border bg-surface/10 px-7 py-2.5 text-xs text-muted">
				<span className="flex items-center gap-1.5">
					<StatusDot state={online > 0 ? "online" : "offline"} />
					<span className="font-mono font-semibold text-text">{online}</span>
					<span className="text-subtle">/ {clients.length} online</span>
				</span>
				{awaiting > 0 && (
					<span className="flex items-center gap-1.5">
						<StatusDot state="pending" />
						<span className="text-primary dark:text-primary-strong">{awaiting} awaiting enrollment</span>
					</span>
				)}
			</div>

			<div className="min-h-0 flex-1 overflow-y-auto px-4 py-4" ref={listRef}>
				{loading ? (
					<p className="px-3 text-sm text-subtle">Loading…</p>
				) : clients.length === 0 ? (
					<EmptyState
						action={
							<Button icon={<Plus className="h-4 w-4" />} onClick={onAdd} variant="primary">
								Add your first machine
							</Button>
						}
						description="Create a machine to get a one-line deploy command. Run it on the target computer to make it remotely controllable."
						icon={<MonitorSmartphone className="h-7 w-7" />}
						title="No machines yet"
					/>
				) : filtered.length === 0 ? (
					<EmptyState
						description="No machine matches your search."
						icon={<Search className="h-6 w-6" />}
						title="Nothing matches"
					/>
				) : (
					<div className="flex flex-col gap-1.5">
						{filtered.map((c, i) => (
							<MachineRow
								actions={actions}
								client={c}
								index={i}
								key={c.id}
								onControl={() => onControl(c)}
								onDetail={() => onDetail(c)}
								onSelect={() => setSelectedId(c.id)}
								onWatch={() => onWatch(c)}
								selected={c.id === selectedId}
							/>
						))}
					</div>
				)}
			</div>
		</>
	)
}

function MachineRow({
	client,
	selected,
	index,
	onSelect,
	onControl,
	onWatch,
	onDetail,
	actions
}: {
	client: ClientDto
	selected: boolean
	index: number
	onSelect: () => void
	onControl: () => void
	onWatch: () => void
	onDetail: () => void
	actions: Actions
}) {
	const { status } = client
	const meta =
		client.online && status
			? `${status.host.hostname} · up ${formatUptime(status.uptime_secs)} · ${status.displays} ${status.displays === 1 ? "display" : "displays"}`
			: !client.enrolled
				? "awaiting enrollment — run the deploy command"
				: `last seen ${relativeTime(client.last_seen)}`

	// Run-command and screenshot act on the machine right here, inline under the row
	// (toggled open), so they don't need the details drawer.
	const [panel, setPanel] = useState<null | "exec" | "shot">(null)
	const [cmd, setCmd] = useState("")
	const [exec, setExec] = useState<ExecResult | null>(null)
	const [shot, setShot] = useState<string | null>(null)
	const execAction = useAsyncAction()
	const shotAction = useAsyncAction()

	const run = () => {
		const parts = tokenizeCommand(cmd)
		if (parts.length === 0) return
		setExec(null)
		execAction.run("Command failed", async () => setExec(await api.clientExec(client.id, parts[0], parts.slice(1))))
	}

	const capture = () =>
		shotAction.run("Screenshot failed", async () => {
			const s = await api.clientScreenshot(client.id)
			setShot(`data:image/png;base64,${s.png_base64}`)
		})

	const toggleExec = () => setPanel((p) => (p === "exec" ? null : "exec"))
	const toggleShot = () => {
		if (panel === "shot") {
			setPanel(null)
			return
		}
		setPanel("shot")
		if (!shot) capture()
	}

	const busy = actions.pending
	const offline = !client.online

	return (
		<div
			className={cn(
				"group flex flex-col rounded-xl border transition",
				selected
					? "border-primary/45 bg-surface-2 ring-1 ring-primary/25"
					: "border-transparent hover:border-border hover:bg-surface-2/60"
			)}
			data-id={client.id}
			style={{ animation: `var(--animate-row-in)`, animationDelay: `${Math.min(index, 12) * 22}ms` }}
		>
			<div className="flex items-center gap-3.5 px-3.5 py-3" onClick={onSelect}>
				<StatusDot className="shrink-0" state={dotState(client)} />

				<div className="grid h-10 w-10 shrink-0 place-items-center rounded-xl bg-surface-3 text-muted">
					<OsIcon className="h-5 w-5" os={client.os} />
				</div>

				<div className="min-w-0 flex-1">
					<div className="truncate font-semibold text-text" title={client.name}>
						{client.name}
					</div>
					<div
						className={cn(
							"truncate font-mono text-[0.76rem]",
							!client.enrolled ? "text-primary dark:text-primary-strong" : "text-subtle"
						)}
					>
						{meta}
					</div>
				</div>

				<div className="flex shrink-0 items-center gap-1.5">
					<Button
						disabled={offline}
						icon={<MonitorUp className="h-4 w-4" />}
						onClick={onControl}
						size="sm"
						variant="primary"
					>
						Control
					</Button>
					<Button
						disabled={offline}
						icon={<Eye className="h-4 w-4" />}
						onClick={onWatch}
						size="sm"
						title="Watch (read-only) (w)"
						variant="outline"
					>
						Watch
					</Button>
					<Button
						disabled={offline || busy[`ssh:${client.id}`]}
						icon={<Terminal className="h-4 w-4" />}
						loading={busy[`ssh:${client.id}`]}
						onClick={() => actions.ssh(client)}
						size="sm"
						title="Connect via SSH (s)"
						variant="outline"
					>
						SSH
					</Button>
					<Button
						disabled={offline || busy[`rdp:${client.id}`]}
						icon={<ScreenShare className="h-4 w-4" />}
						loading={busy[`rdp:${client.id}`]}
						onClick={() => actions.rdp(client)}
						size="sm"
						title="Connect via RDP (r)"
						variant="outline"
					>
						RDP
					</Button>
					<div
						className={cn(
							"ml-0.5 flex items-center gap-1 transition",
							selected ? "opacity-100" : "opacity-60 group-hover:opacity-100"
						)}
					>
						<Button
							className={cn(panel === "exec" && "bg-surface-3 text-text")}
							disabled={offline}
							icon={<SquareChevronRight className="h-4 w-4" />}
							onClick={toggleExec}
							size="icon-sm"
							title="Run a command"
							variant="ghost"
						/>
						<Button
							className={cn(panel === "shot" && "bg-surface-3 text-text")}
							disabled={offline}
							icon={<Camera className="h-4 w-4" />}
							onClick={toggleShot}
							size="icon-sm"
							title="Screenshot"
							variant="ghost"
						/>
						<Menu
							items={[
								{
									disabled: offline,
									icon: <Info className="h-4 w-4" />,
									key: "info",
									label: "Info",
									onSelect: onDetail
								},
								{
									icon: <Rocket className="h-4 w-4" />,
									key: "deploy",
									label: client.enrolled ? "Re-deploy…" : "Get deploy command",
									onSelect: () => actions.deploy(client)
								},
								{
									danger: true,
									icon: <Trash2 className="h-4 w-4" />,
									key: "remove",
									label: "Delete",
									onSelect: () => actions.remove(client)
								}
							]}
						/>
					</div>
				</div>
			</div>

			{panel === "exec" && (
				<div className="border-border border-t px-3.5 pt-3 pb-3.5">
					<div className="flex gap-2">
						<Input
							autoFocus
							className="flex-1 font-mono"
							onChange={(e) => setCmd(e.target.value)}
							onKeyDown={(e) => {
								if (e.key === "Enter") run()
								if (e.key === "Escape") setPanel(null)
							}}
							placeholder="e.g. uname -a"
							value={cmd}
						/>
						<Button
							icon={<Play className="h-4 w-4" />}
							loading={execAction.busy}
							onClick={run}
							size="md"
							variant="primary"
						>
							Run
						</Button>
					</div>
					{exec && (
						<div className="mt-2 rounded-xl border border-border bg-surface-2 p-3 text-xs">
							<div className="mb-1.5 font-mono text-subtle">
								exit {exec.code ?? "—"} · {exec.duration_ms}ms
							</div>
							{exec.stdout && (
								<pre className="select-text overflow-auto whitespace-pre-wrap font-mono text-text">
									{exec.stdout}
								</pre>
							)}
							{exec.stderr && (
								<pre className="select-text overflow-auto whitespace-pre-wrap font-mono text-danger">
									{exec.stderr}
								</pre>
							)}
						</div>
					)}
				</div>
			)}

			{panel === "shot" && (
				<div className="border-border border-t px-3.5 pt-3 pb-3.5">
					<div className="mb-2 flex items-center justify-between">
						<div className="eyebrow">Screenshot</div>
						<div className="flex items-center gap-1.5">
							<Button
								icon={<Camera className="h-3.5 w-3.5" />}
								loading={shotAction.busy}
								onClick={capture}
								size="sm"
								variant="ghost"
							>
								Recapture
							</Button>
							<Button
								icon={<X className="h-3.5 w-3.5" />}
								onClick={() => setPanel(null)}
								size="icon-sm"
								title="Close"
								variant="ghost"
							/>
						</div>
					</div>
					{shotAction.busy && !shot ? (
						<div className="grid h-40 place-items-center rounded-xl border border-border bg-surface-2">
							<Spinner className="h-5 w-5" />
						</div>
					) : shot ? (
						<img alt="remote screen" className="w-full rounded-xl border border-border" src={shot} />
					) : (
						<p className="text-xs text-subtle">No capture yet.</p>
					)}
				</div>
			)}
		</div>
	)
}
