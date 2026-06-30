import { CornerDownLeft, Search } from "lucide-react"
import { type ReactNode, useEffect, useMemo, useRef, useState } from "react"
import { cn } from "../lib/cn"
import { fuzzy } from "./Combobox"
import { Kbd } from "./ui"

export interface Command {
	id: string
	label: string
	/** Section heading the command is grouped under (e.g. "Navigate", "Machines"). */
	group: string
	icon: ReactNode
	/** Muted right-aligned context — a hostname, "offline", a shortcut. */
	hint?: ReactNode
	/** Extra text folded into the search (not shown). */
	keywords?: string
	disabled?: boolean
	run: () => void
}

/** A ⌘K command bar: fuzzy-search every machine and action from one place, drive
 *  it entirely from the keyboard, and act without hunting through the UI. This is
 *  the app's "fewer steps" backbone. */
export function CommandPalette({
	open,
	onClose,
	commands
}: {
	open: boolean
	onClose: () => void
	commands: Command[]
}) {
	const [query, setQuery] = useState("")
	const [active, setActive] = useState(0)
	const inputRef = useRef<HTMLInputElement>(null)
	const listRef = useRef<HTMLDivElement>(null)

	// Fresh, empty bar each time it opens.
	useEffect(() => {
		if (open) {
			setQuery("")
			setActive(0)
			// Focus after paint so the input is mounted.
			requestAnimationFrame(() => inputRef.current?.focus())
		}
	}, [open])

	const results = useMemo(() => {
		const q = query.trim()
		const scored = commands
			.map((c) => {
				const base = q ? (fuzzy(q, `${c.label} ${c.keywords ?? ""}`)?.score ?? null) : 0
				// De-rank unavailable actions (e.g. an offline machine's controls) so a
				// generic query like "ssh" surfaces reachable machines first.
				return { cmd: c, score: base === null ? null : base - (c.disabled ? 6 : 0) }
			})
			.filter((r): r is { cmd: Command; score: number } => r.score !== null)
		if (q) scored.sort((a, b) => b.score - a.score)
		return scored.map((r) => r.cmd)
	}, [commands, query])

	// Keep the highlight within bounds as the result list shrinks.
	useEffect(() => {
		setActive((a) => Math.min(a, Math.max(0, results.length - 1)))
	}, [results.length])

	useEffect(() => {
		listRef.current?.querySelector<HTMLElement>(`[data-idx="${active}"]`)?.scrollIntoView({ block: "nearest" })
	}, [active])

	if (!open) return null

	const choose = (cmd: Command) => {
		if (cmd.disabled) return
		onClose()
		cmd.run()
	}

	const onKeyDown = (e: React.KeyboardEvent) => {
		if (e.key === "ArrowDown") {
			e.preventDefault()
			setActive((a) => Math.min(results.length - 1, a + 1))
		} else if (e.key === "ArrowUp") {
			e.preventDefault()
			setActive((a) => Math.max(0, a - 1))
		} else if (e.key === "Enter") {
			e.preventDefault()
			const cmd = results[active]
			if (cmd) choose(cmd)
		} else if (e.key === "Escape") {
			e.preventDefault()
			onClose()
		}
	}

	// Group in the order groups first appear in the (sorted) results.
	const groups: { name: string; items: { cmd: Command; idx: number }[] }[] = []
	results.forEach((cmd, idx) => {
		let g = groups.find((x) => x.name === cmd.group)
		if (!g) {
			g = { items: [], name: cmd.group }
			groups.push(g)
		}
		g.items.push({ cmd, idx })
	})

	return (
		<div
			className="fixed inset-0 z-[80] flex justify-center px-5 pt-[12vh]"
			style={{ animation: "var(--animate-fade-in)" }}
		>
			<div className="absolute inset-0 bg-black/55 backdrop-blur-sm" onClick={onClose} />
			<div
				aria-modal="true"
				className="card relative z-10 flex h-fit max-h-[68vh] w-full max-w-xl flex-col overflow-hidden shadow-2xl shadow-black/50"
				role="dialog"
				style={{ animation: "var(--animate-slide-up)" }}
			>
				<div className="flex items-center gap-3 border-b border-border px-4 py-3">
					<Search className="h-4.5 w-4.5 shrink-0 text-subtle" />
					<input
						className="no-drag w-full bg-transparent text-[0.95rem] text-text outline-none placeholder:text-subtle"
						onChange={(e) => {
							setQuery(e.target.value)
							setActive(0)
						}}
						onKeyDown={onKeyDown}
						placeholder="Search machines and actions…"
						ref={inputRef}
						value={query}
					/>
					<Kbd>esc</Kbd>
				</div>

				<div className="min-h-0 flex-1 overflow-y-auto p-1.5" ref={listRef}>
					{results.length === 0 ? (
						<p className="px-3 py-8 text-center text-sm text-subtle">No matches for "{query}"</p>
					) : (
						groups.map((g) => (
							<div className="mb-1" key={g.name}>
								<div className="eyebrow px-3 pb-1 pt-2">{g.name}</div>
								{g.items.map(({ cmd, idx }) => (
									<button
										className={cn(
											"flex w-full items-center gap-3 rounded-lg px-3 py-2 text-left text-sm transition",
											cmd.disabled && "opacity-40",
											idx === active ? "bg-surface-3 text-text" : "text-muted hover:bg-surface-2"
										)}
										data-idx={idx}
										disabled={cmd.disabled}
										key={cmd.id}
										onClick={() => choose(cmd)}
										onMouseMove={() => setActive(idx)}
										type="button"
									>
										<span
											className={cn(
												"flex h-5 w-5 shrink-0 items-center justify-center",
												idx === active ? "text-primary dark:text-primary-strong" : "text-subtle"
											)}
										>
											{cmd.icon}
										</span>
										<span className="min-w-0 flex-1 truncate text-text">{cmd.label}</span>
										{cmd.hint && (
											<span className="shrink-0 font-mono text-[0.72rem] text-subtle">
												{cmd.hint}
											</span>
										)}
										{idx === active && (
											<CornerDownLeft className="h-3.5 w-3.5 shrink-0 text-subtle" />
										)}
									</button>
								))}
							</div>
						))
					)}
				</div>
			</div>
		</div>
	)
}
