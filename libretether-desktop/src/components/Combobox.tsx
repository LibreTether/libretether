import { Check, ChevronsUpDown, Search } from "lucide-react"
import { type KeyboardEvent, type ReactNode, useEffect, useMemo, useRef, useState } from "react"
import { cn } from "../lib/cn"

export interface ComboOption {
	value: string
	label?: string
	/** Optional leading icon, shown in both the trigger and the dropdown row. */
	icon?: ReactNode
}

interface Match {
	opt: ComboOption
	label: string
	score: number
	ranges: Set<number>
}

/** Fuzzy subsequence match with light scoring; `null` when the query can't match. */
function fuzzy(query: string, label: string): { score: number; ranges: Set<number> } | null {
	const q = query.toLowerCase()
	const t = label.toLowerCase()
	if (!q) return { ranges: new Set(), score: 0 }
	const ranges = new Set<number>()
	let from = 0
	let prev = -2
	let score = 0
	for (const ch of q) {
		const at = t.indexOf(ch, from)
		if (at === -1) return null
		ranges.add(at)
		if (at === prev + 1) score += 8 // consecutive
		if (at === 0 || /[\s\-_./]/.test(t[at - 1] ?? "")) score += 6 // word start
		score += 1
		prev = at
		from = at + 1
	}
	if (t.includes(q)) score += 25
	if (t.startsWith(q)) score += 15
	score += Math.max(0, 12 - label.length / 4)
	return { ranges, score }
}

function highlight(label: string, ranges: Set<number>): ReactNode {
	if (ranges.size === 0) return label
	return [...label].map((ch, i) =>
		ranges.has(i) ? (
			<span className="font-semibold text-primary dark:text-primary-strong" key={i}>
				{ch}
			</span>
		) : (
			ch
		)
	)
}

/** A searchable, keyboard-navigable, fuzzy-matching select. Drop-in replacement
 *  for a native `<select>` across the app. */
export function Combobox({
	value,
	onChange,
	options,
	placeholder = "Select…",
	searchPlaceholder = "Search…",
	emptyText = "No options",
	noMatchText = "No matches",
	disabled,
	loading,
	className
}: {
	value: string | null
	onChange: (value: string) => void
	options: ComboOption[]
	placeholder?: string
	searchPlaceholder?: string
	emptyText?: string
	noMatchText?: string
	disabled?: boolean
	loading?: boolean
	className?: string
}) {
	const [open, setOpen] = useState(false)
	const [query, setQuery] = useState("")
	const [active, setActive] = useState(0)
	const rootRef = useRef<HTMLDivElement>(null)
	const inputRef = useRef<HTMLInputElement>(null)
	const listRef = useRef<HTMLDivElement>(null)

	const selected = useMemo(() => options.find((o) => o.value === value) ?? null, [options, value])
	const selectedLabel = selected?.label ?? value

	const matches = useMemo<Match[]>(() => {
		const out: Match[] = []
		for (const opt of options) {
			const label = opt.label ?? opt.value
			const m = fuzzy(query, label)
			if (m) out.push({ label, opt, ranges: m.ranges, score: m.score })
		}
		out.sort((a, b) => b.score - a.score || a.label.localeCompare(b.label))
		return out
	}, [options, query])

	useEffect(() => {
		setActive(0)
	}, [])

	useEffect(() => {
		if (open) inputRef.current?.focus()
	}, [open])

	useEffect(() => {
		if (!open) return
		const onDown = (e: MouseEvent) => {
			if (rootRef.current && !rootRef.current.contains(e.target as Node)) setOpen(false)
		}
		window.addEventListener("mousedown", onDown)
		return () => window.removeEventListener("mousedown", onDown)
	}, [open])

	useEffect(() => {
		listRef.current?.querySelector<HTMLElement>(`[data-idx="${active}"]`)?.scrollIntoView({ block: "nearest" })
	}, [active])

	const choose = (v: string) => {
		onChange(v)
		setOpen(false)
		setQuery("")
	}

	const onKeyDown = (e: KeyboardEvent) => {
		if (e.key === "ArrowDown") {
			e.preventDefault()
			setActive((a) => Math.min(matches.length - 1, a + 1))
		} else if (e.key === "ArrowUp") {
			e.preventDefault()
			setActive((a) => Math.max(0, a - 1))
		} else if (e.key === "Enter") {
			e.preventDefault()
			const m = matches[active]
			if (m) choose(m.opt.value)
		} else if (e.key === "Escape") {
			e.preventDefault()
			setOpen(false)
		}
	}

	const empty = options.length === 0

	return (
		<div className={cn("relative", className)} ref={rootRef}>
			<button
				className={cn(
					"no-drag flex w-full items-center gap-2 rounded-lg border border-border bg-surface-2 px-3 py-2 text-sm outline-none transition focus:border-primary focus:ring-2 focus:ring-ring/30 disabled:opacity-50",
					selectedLabel ? "text-text" : "text-subtle"
				)}
				disabled={disabled || empty}
				onClick={() => setOpen((o) => !o)}
				type="button"
			>
				{!loading && !empty && selected?.icon && (
					<span className="flex shrink-0 items-center">{selected.icon}</span>
				)}
				<span className="min-w-0 flex-1 truncate text-left">
					{loading ? "Applying…" : empty ? emptyText : (selectedLabel ?? placeholder)}
				</span>
				<ChevronsUpDown className="h-4 w-4 shrink-0 text-subtle" />
			</button>

			{open && !empty && (
				<div
					className="card absolute z-30 mt-1.5 w-full overflow-hidden p-0 shadow-xl shadow-black/20"
					style={{ animation: "var(--animate-fade-in)" }}
				>
					<div className="flex items-center gap-2 border-b border-border px-3 py-2">
						<Search className="h-4 w-4 shrink-0 text-subtle" />
						<input
							className="w-full bg-transparent text-sm text-text outline-none placeholder:text-subtle"
							onChange={(e) => setQuery(e.target.value)}
							onKeyDown={onKeyDown}
							placeholder={searchPlaceholder}
							ref={inputRef}
							value={query}
						/>
					</div>
					<div className="max-h-64 overflow-y-auto p-1.5" ref={listRef}>
						{matches.length === 0 ? (
							<p className="px-2.5 py-3 text-center text-xs text-subtle">{noMatchText}</p>
						) : (
							matches.map((m, i) => (
								<button
									className={cn(
										"flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left text-sm transition",
										i === active
											? "bg-surface-3 text-text"
											: "text-muted hover:bg-surface-3 hover:text-text"
									)}
									data-idx={i}
									key={m.opt.value}
									onClick={() => choose(m.opt.value)}
									onMouseMove={() => setActive(i)}
									type="button"
								>
									{m.opt.icon && <span className="flex shrink-0 items-center">{m.opt.icon}</span>}
									<span className="min-w-0 flex-1 truncate">{highlight(m.label, m.ranges)}</span>
									{m.opt.value === value && <Check className="h-4 w-4 shrink-0 text-primary" />}
								</button>
							))
						)}
					</div>
				</div>
			)}
		</div>
	)
}
