import { Eraser, RotateCw, Search } from "lucide-react"
import { useCallback, useEffect, useMemo, useRef, useState } from "react"
import { Combobox } from "../components/Combobox"
import { PageHeader } from "../components/PageHeader"
import { Button, EmptyState, Input } from "../components/ui"
import * as api from "../lib/api"
import { cn } from "../lib/cn"
import { useHotkeys } from "../lib/hotkeys"
import { useToast } from "../lib/toast"
import type { ClientDto, LogEntry, LogLevel } from "../lib/types"

const LEVELS: LogLevel[] = ["error", "warn", "info", "debug", "trace"]

/** Sources produced by the controller itself (its own buffer) — everything else is
 *  an agent's name (or the relay). Used so a refresh of the controller buffer
 *  replaces these in place without duplicating them, while leaving on-demand agent
 *  and relay logs untouched. */
const CONTROLLER_SOURCES = new Set(["controller", "tunnel", "rdp"])

/** Source tag for lines pulled from the relay server itself (relay mode only). */
const RELAY_SOURCE = "relay"

/** Keep the view bounded — a live controller can emit indefinitely. */
const MAX_ROWS = 3000

/** Per-level text colour, reused by the log rows and the filter pills. */
const LEVEL_TEXT: Record<LogLevel, string> = {
	debug: "text-muted",
	error: "text-danger",
	info: "text-accent",
	trace: "text-subtle",
	warn: "text-warning"
}

const LEVEL_PILL: Record<LogLevel, string> = {
	debug: "bg-surface-3 text-muted",
	error: "bg-danger-soft text-danger",
	info: "bg-accent/12 text-accent",
	trace: "bg-surface-3 text-subtle",
	warn: "bg-warning-soft text-warning"
}

type Row = LogEntry & { _id: number }

function formatTime(tsSecs: number): string {
	return new Date(tsSecs * 1000).toLocaleTimeString(undefined, { hour12: false })
}

/** Drop the oldest rows once the view exceeds [`MAX_ROWS`]. Module-level so it's a
 *  stable reference (not a hook dependency). */
function cap(r: Row[]): Row[] {
	return r.length > MAX_ROWS ? r.slice(r.length - MAX_ROWS) : r
}

export function LogsPage({
	clients,
	hotkeysEnabled,
	relayMode
}: {
	clients: ClientDto[]
	hotkeysEnabled: boolean
	/** True when the active controller runs in relay mode — gates the "relay logs"
	 *  control, since only then is there a relay server to pull logs from. */
	relayMode: boolean
}) {
	const toast = useToast()
	const [rows, setRows] = useState<Row[]>([])
	const [levels, setLevels] = useState<Set<LogLevel>>(() => new Set(LEVELS))
	const [source, setSource] = useState<string>("all")
	const [search, setSearch] = useState("")
	const [agentId, setAgentId] = useState("")
	const [fetching, setFetching] = useState(false)
	const [relayFetching, setRelayFetching] = useState(false)
	const searchRef = useRef<HTMLInputElement>(null)

	useHotkeys([{ combo: "/", handler: () => searchRef.current?.focus() }], hotkeysEnabled)

	const idRef = useRef(0)
	const tag = useCallback((es: LogEntry[]): Row[] => es.map((e) => ({ ...e, _id: idRef.current++ })), [])

	// Seed from the controller's buffer, then stream new lines live.
	useEffect(() => {
		let alive = true
		api.getControllerLogs()
			.then((es) => {
				if (alive) setRows(tag(es))
			})
			.catch(() => {
				/* the page still works for agent logs */
			})
		const unlisten = api.onLogEntry((e) => {
			const [r] = tag([e])
			setRows((prev) => cap([...prev, r]))
		})
		return () => {
			alive = false
			unlisten.then((fn) => fn())
		}
	}, [tag])

	const online = useMemo(() => clients.filter((c) => c.online), [clients])

	// Default the agent picker to the first online client, and keep it valid as
	// machines come and go.
	useEffect(() => {
		setAgentId((cur) => (online.some((c) => c.id === cur) ? cur : (online[0]?.id ?? "")))
	}, [online])

	const loadAgentLogs = async () => {
		const client = clients.find((c) => c.id === agentId)
		if (!client) return
		setFetching(true)
		try {
			const fresh = tag(await api.clientLogs(client.id))
			// Replace any previously-loaded lines for this machine so re-fetching
			// doesn't pile up duplicates.
			setRows((prev) => cap([...prev.filter((r) => r.source !== client.name), ...fresh]))
			if (fresh.length === 0) toast.info("No agent logs", `${client.name} reported an empty log buffer.`)
		} catch (e) {
			toast.error("Couldn't fetch agent logs", api.errString(e))
		} finally {
			setFetching(false)
		}
	}

	const loadRelayLogs = async () => {
		setRelayFetching(true)
		try {
			const fresh = tag(await api.relayLogs())
			// Replace any previously-loaded relay lines so re-fetching doesn't pile up
			// duplicates (the relay is a single source, like one agent).
			setRows((prev) => cap([...prev.filter((r) => r.source !== RELAY_SOURCE), ...fresh]))
			if (fresh.length === 0) toast.info("No relay logs", "The relay reported an empty log buffer.")
		} catch (e) {
			toast.error("Couldn't fetch relay logs", api.errString(e))
		} finally {
			setRelayFetching(false)
		}
	}

	const refreshController = async () => {
		try {
			const ctrl = tag(await api.getControllerLogs())
			setRows((prev) => cap([...ctrl, ...prev.filter((r) => !CONTROLLER_SOURCES.has(r.source))]))
		} catch (e) {
			toast.error("Couldn't refresh logs", api.errString(e))
		}
	}

	const sources = useMemo(() => {
		const set = new Set(rows.map((r) => r.source))
		return [...set].sort()
	}, [rows])

	const filtered = useMemo(() => {
		const q = search.trim().toLowerCase()
		return rows
			.filter((r) => levels.has(r.level))
			.filter((r) => source === "all" || r.source === source)
			.filter((r) => !q || r.message.toLowerCase().includes(q) || r.source.toLowerCase().includes(q))
			.slice()
			.sort((a, b) => a.ts_secs - b.ts_secs || a._id - b._id)
	}, [rows, levels, source, search])

	// Follow the tail: stay pinned to the bottom unless the user has scrolled up.
	const scrollRef = useRef<HTMLDivElement>(null)
	const stick = useRef(true)
	const onScroll = () => {
		const el = scrollRef.current
		if (el) stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40
	}
	// biome-ignore lint/correctness/useExhaustiveDependencies: re-run to scroll to the tail whenever the rendered rows change.
	useEffect(() => {
		const el = scrollRef.current
		if (el && stick.current) el.scrollTop = el.scrollHeight
	}, [filtered])

	const toggleLevel = (level: LogLevel) => {
		setLevels((prev) => {
			const next = new Set(prev)
			if (next.has(level)) next.delete(level)
			else next.add(level)
			return next
		})
	}

	return (
		<div className="flex h-full min-h-0 flex-col">
			<PageHeader
				actions={
					<>
						<Button
							icon={<RotateCw className="h-4 w-4" />}
							onClick={refreshController}
							size="sm"
							variant="soft"
						>
							Refresh
						</Button>
						<Button
							icon={<Eraser className="h-4 w-4" />}
							onClick={() => setRows([])}
							size="sm"
							variant="ghost"
						>
							Clear
						</Button>
					</>
				}
				eyebrow="Activity"
				subtitle="Live controller activity, plus agent logs you pull from a machine"
				title="Logs"
			/>

			{/* Toolbar: search, level pills, source + agent-log controls. */}
			<div className="no-drag flex flex-col gap-3 border-b border-border bg-surface/20 px-7 py-3">
				<div className="flex items-center gap-3">
					<div className="relative min-w-0 flex-1">
						<Search className="-translate-y-1/2 pointer-events-none absolute top-1/2 left-3 h-4 w-4 text-subtle" />
						<Input
							className="pl-9"
							onChange={(e) => setSearch(e.target.value)}
							placeholder="Search messages…"
							ref={searchRef}
							value={search}
						/>
					</div>
					<div className="flex shrink-0 items-center gap-1.5">
						{LEVELS.map((level) => {
							const on = levels.has(level)
							return (
								<button
									className={cn(
										"no-drag rounded-full px-2.5 py-1 font-mono text-xs font-semibold uppercase tracking-wide transition",
										on ? LEVEL_PILL[level] : "bg-surface-2 text-subtle line-through opacity-60"
									)}
									key={level}
									onClick={() => toggleLevel(level)}
									type="button"
								>
									{level}
								</button>
							)
						})}
					</div>
				</div>

				<div className="flex flex-wrap items-center gap-2 text-sm">
					<div className="flex items-center gap-1.5 text-muted">
						<span className="eyebrow">Source</span>
						<Combobox
							className="w-44"
							onChange={setSource}
							options={[{ label: "All sources", value: "all" }, ...sources.map((s) => ({ value: s }))]}
							value={source}
						/>
					</div>

					<div className="ml-auto flex items-center gap-4">
						{/* Relay-mode only: the relay server has its own log we can pull. */}
						{relayMode && (
							<div className="flex items-center gap-2">
								<span className="eyebrow">Relay</span>
								<Button loading={relayFetching} onClick={loadRelayLogs} size="sm" variant="soft">
									Load relay logs
								</Button>
							</div>
						)}
						<div className="flex items-center gap-2">
							<span className="eyebrow">Agent logs</span>
							<Combobox
								className="w-44"
								disabled={online.length === 0}
								emptyText="No machines online"
								onChange={setAgentId}
								options={online.map((c) => ({ label: c.name, value: c.id }))}
								placeholder="Pick a machine…"
								value={agentId || null}
							/>
							<Button
								disabled={online.length === 0 || !agentId}
								loading={fetching}
								onClick={loadAgentLogs}
								size="sm"
								variant="soft"
							>
								Load
							</Button>
						</div>
					</div>
				</div>
			</div>

			{/* Log rows. */}
			<div
				className="min-h-0 flex-1 select-text overflow-y-auto px-4 py-2 font-mono text-xs"
				onScroll={onScroll}
				ref={scrollRef}
			>
				{filtered.length === 0 ? (
					<div className="grid h-full place-items-center">
						<EmptyState
							description={
								rows.length === 0
									? "Controller activity will appear here. Use “Load” to pull a machine's agent log."
									: "Adjust the level filters, source, or search."
							}
							icon={<Search className="h-6 w-6" />}
							title={rows.length === 0 ? "No logs yet" : "Nothing matches"}
						/>
					</div>
				) : (
					filtered.map((r) => (
						<div className="flex gap-3 rounded px-2 py-0.5 hover:bg-surface-2/60" key={r._id}>
							<span className="shrink-0 text-subtle tabular-nums">{formatTime(r.ts_secs)}</span>
							<span className={cn("w-12 shrink-0 font-semibold uppercase", LEVEL_TEXT[r.level])}>
								{r.level}
							</span>
							<span className="w-28 shrink-0 truncate text-muted" title={r.source}>
								{r.source}
							</span>
							<span className="min-w-0 whitespace-pre-wrap break-words text-text">{r.message}</span>
						</div>
					))
				)}
			</div>
		</div>
	)
}
