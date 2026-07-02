import { ArrowUp, File, Folder, HardDrive, Home, Link2, RefreshCw } from "lucide-react"
import { type ReactNode, useCallback, useEffect, useState } from "react"
import { cn } from "../lib/cn"
import { formatBytes, relativeTime } from "../lib/format"
import type { DirEntry, DirListing } from "../lib/types"
import { Spinner } from "./ui"

/** A file/dir the user picked to transfer, by absolute path. */
export interface SelectedEntry {
	path: string
	name: string
	isDir: boolean
}

/** Join an absolute base path with a child name, inferring the OS separator from the
 *  base so remote Windows paths (browsed from a Linux controller) still work — the
 *  backend canonicalizes the result regardless. */
function joinPath(base: string, name: string): string {
	const isWin = /^[A-Za-z]:/.test(base) || base.includes("\\")
	const sep = isWin ? "\\" : "/"
	return base.endsWith(sep) ? base + name : base + sep + name
}

function EntryIcon({ kind }: { kind: DirEntry["kind"] }) {
	if (kind === "dir") return <Folder className="h-4 w-4 text-primary dark:text-primary-strong" />
	if (kind === "symlink") return <Link2 className="h-4 w-4 text-accent" />
	return <File className="h-4 w-4 text-subtle" />
}

/**
 * One pane of the transfer browser. Drives its own current-directory listing via
 * `load` (a remote or local lister), so the same component serves both sides. In
 * `selectable` mode each row has a checkbox (files *and* directories can be picked, for
 * recursive folder transfer); otherwise the pane is a destination picker and the
 * current directory is reported via `onPathChange`.
 */
export function FileBrowser({
	title,
	icon,
	load,
	selectable,
	selected,
	onToggle,
	onPathChange
}: {
	title: ReactNode
	icon?: ReactNode
	load: (path: string | null) => Promise<DirListing>
	selectable: boolean
	selected?: SelectedEntry[]
	onToggle?: (entry: SelectedEntry) => void
	onPathChange?: (path: string) => void
}) {
	const [listing, setListing] = useState<DirListing | null>(null)
	const [loading, setLoading] = useState(true)
	const [error, setError] = useState<string | null>(null)

	const go = useCallback(
		(path: string | null) => {
			setLoading(true)
			setError(null)
			load(path)
				.then((l) => {
					setListing(l)
					onPathChange?.(l.path)
				})
				.catch((e) => setError(typeof e === "string" ? e : (e?.message ?? "Couldn't read directory")))
				.finally(() => setLoading(false))
		},
		[load, onPathChange]
	)

	// Seed with the home directory + roots on mount (and whenever the lister changes,
	// e.g. the direction toggle swaps which side is remote).
	useEffect(() => {
		go(null)
	}, [go])

	const isSelected = (path: string) => !!selected?.some((s) => s.path === path)

	return (
		<div className="flex min-h-0 flex-1 flex-col rounded-xl border border-border bg-surface-2/40">
			<div className="flex items-center gap-2 border-b border-border px-3 py-2">
				{icon}
				<span className="text-xs font-semibold text-muted">{title}</span>
				<div className="ml-auto flex items-center gap-1">
					<button
						aria-label="Up"
						className="no-drag rounded-md p-1 text-subtle transition enabled:hover:bg-surface-3 enabled:hover:text-text disabled:opacity-40"
						disabled={!listing?.parent}
						onClick={() => listing?.parent && go(listing.parent)}
						type="button"
					>
						<ArrowUp className="h-4 w-4" />
					</button>
					<button
						aria-label="Home"
						className="no-drag rounded-md p-1 text-subtle transition hover:bg-surface-3 hover:text-text"
						onClick={() => go(null)}
						type="button"
					>
						<Home className="h-4 w-4" />
					</button>
					<button
						aria-label="Refresh"
						className="no-drag rounded-md p-1 text-subtle transition hover:bg-surface-3 hover:text-text"
						onClick={() => go(listing?.path ?? null)}
						type="button"
					>
						<RefreshCw className="h-4 w-4" />
					</button>
				</div>
			</div>

			{/* Current path + any roots to jump to. */}
			<div className="flex flex-wrap items-center gap-1 border-b border-border px-3 py-1.5">
				<span className="truncate font-mono text-[0.7rem] text-subtle" title={listing?.path}>
					{listing?.path ?? "…"}
				</span>
				{listing?.roots.map((root) => (
					<button
						className="no-drag inline-flex items-center gap-1 rounded-md bg-surface-3 px-1.5 py-0.5 text-[0.68rem] text-muted transition hover:text-text"
						key={root}
						onClick={() => go(root)}
						type="button"
					>
						<HardDrive className="h-3 w-3" />
						{root}
					</button>
				))}
			</div>

			<div className="h-64 min-h-0 flex-1 overflow-y-auto">
				{loading ? (
					<div className="grid h-full place-items-center text-subtle">
						<Spinner />
					</div>
				) : error ? (
					<div className="p-3 text-xs text-danger">{error}</div>
				) : listing && listing.entries.length === 0 ? (
					<div className="grid h-full place-items-center text-xs text-subtle">Empty folder</div>
				) : (
					<ul className="divide-y divide-border/60">
						{listing?.entries.map((entry) => {
							const full = joinPath(listing.path, entry.name)
							const picked = isSelected(full)
							return (
								<li
									className={cn(
										"flex items-center gap-2 px-2.5 py-1.5 text-sm",
										picked && "bg-primary-soft/40"
									)}
									key={entry.name}
								>
									{selectable && (
										<input
											aria-label={`Select ${entry.name}`}
											checked={picked}
											className="no-drag h-3.5 w-3.5 shrink-0 accent-[var(--primary)]"
											onChange={() =>
												onToggle?.({
													isDir: entry.kind === "dir",
													name: entry.name,
													path: full
												})
											}
											type="checkbox"
										/>
									)}
									<EntryIcon kind={entry.kind} />
									<button
										className={cn(
											"no-drag min-w-0 flex-1 truncate text-left",
											entry.kind === "dir" ? "text-text hover:underline" : "text-muted"
										)}
										onClick={() =>
											entry.kind === "dir"
												? go(full)
												: selectable &&
													onToggle?.({ isDir: false, name: entry.name, path: full })
										}
										title={entry.name}
										type="button"
									>
										{entry.name}
									</button>
									{entry.kind !== "dir" && (
										<span className="shrink-0 text-[0.7rem] text-subtle">
											{formatBytes(entry.size)}
										</span>
									)}
									{entry.mtime != null && (
										<span className="hidden shrink-0 text-[0.7rem] text-subtle sm:inline">
											{relativeTime(entry.mtime)}
										</span>
									)}
								</li>
							)
						})}
					</ul>
				)}
			</div>
		</div>
	)
}
