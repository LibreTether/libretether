import { LogOut, MonitorSmartphone, Plug, type Radio, ScrollText, Search } from "lucide-react"
import { cn } from "../lib/cn"
import { MOD_LABEL } from "../lib/hotkeys"
import { CONTROLLER_TYPE_META } from "../lib/meta"
import type { ActiveInfo } from "../lib/types"
import { Kbd } from "./ui"

export type Page = "machines" | "controller" | "logs"

const NAV: { id: Page; label: string; icon: typeof Radio; key: string }[] = [
	{ icon: MonitorSmartphone, id: "machines", key: "1", label: "Machines" },
	{ icon: Plug, id: "controller", key: "2", label: "Connection" },
	{ icon: ScrollText, id: "logs", key: "3", label: "Logs" }
]

export function Sidebar({
	active,
	page,
	onNavigate,
	onExit,
	onOpenPalette,
	onOpenShortcuts
}: {
	active: ActiveInfo
	page: Page
	onNavigate: (p: Page) => void
	onExit: () => void
	onOpenPalette: () => void
	onOpenShortcuts: () => void
}) {
	return (
		<aside className="flex w-60 shrink-0 flex-col border-r border-border bg-surface/40">
			<div className="drag flex items-center gap-2.5 px-5 pb-3 pt-6">
				<img alt="" className="h-7 w-7 rounded-lg" src="/libretether.png" />
				<span className="font-display text-[0.95rem] font-bold tracking-tight text-text">LibreTether</span>
			</div>

			<div className="px-3 pb-2">
				<div className="no-drag flex items-center gap-2.5 rounded-xl border border-border bg-surface-2/70 px-3 py-2">
					<span className="grid h-7 w-7 shrink-0 place-items-center rounded-lg bg-surface-3 text-primary dark:text-primary-strong">
						{(() => {
							const Icon = CONTROLLER_TYPE_META[active.kind.type].icon
							return <Icon className="h-3.5 w-3.5" />
						})()}
					</span>
					<div className="min-w-0 leading-tight">
						<div className="truncate text-[0.82rem] font-semibold text-text">{active.name}</div>
						<div className="eyebrow">{CONTROLLER_TYPE_META[active.kind.type].label}</div>
					</div>
				</div>
			</div>

			<div className="px-3 pb-1">
				<button
					className="no-drag flex w-full items-center gap-2.5 rounded-lg border border-border bg-surface-2/50 px-3 py-2 text-sm text-subtle transition hover:border-border-strong hover:text-muted"
					onClick={onOpenPalette}
					type="button"
				>
					<Search className="h-4 w-4" />
					<span className="flex-1 text-left">Search…</span>
					<Kbd>{MOD_LABEL}</Kbd>
					<Kbd>K</Kbd>
				</button>
			</div>

			<nav className="flex flex-col gap-1 px-3 py-2">
				{NAV.map((item) => {
					const isActive = page === item.id
					const Icon = item.icon
					return (
						<button
							className={cn(
								"no-drag group relative flex items-center gap-3 rounded-xl px-3 py-2.5 text-sm font-medium transition",
								isActive
									? "bg-primary-soft text-primary dark:text-primary-strong"
									: "text-muted hover:bg-surface-3 hover:text-text"
							)}
							key={item.id}
							onClick={() => onNavigate(item.id)}
						>
							{isActive && (
								<span className="-translate-y-1/2 absolute top-1/2 left-0 h-5 w-0.5 rounded-full bg-primary dark:bg-primary-strong" />
							)}
							<Icon className="h-4.5 w-4.5" />
							<span className="flex-1 text-left">{item.label}</span>
							<span
								className={cn(
									"font-mono text-[0.7rem] transition",
									isActive
										? "text-primary/70 dark:text-primary-strong/70"
										: "text-subtle opacity-0 group-hover:opacity-100"
								)}
							>
								{item.key}
							</span>
						</button>
					)
				})}
			</nav>

			<div className="mt-auto flex flex-col gap-1.5 px-3 pb-4">
				<button
					className="no-drag flex items-center gap-3 rounded-xl px-3 py-2 text-sm font-medium text-muted transition hover:bg-surface-3 hover:text-text"
					onClick={onOpenShortcuts}
					type="button"
				>
					<span className="kbd">?</span>
					<span>Shortcuts</span>
				</button>
				<button
					className="no-drag flex items-center gap-3 rounded-xl px-3 py-2 text-sm font-medium text-muted transition hover:bg-danger-soft hover:text-danger"
					onClick={onExit}
				>
					<LogOut className="h-4.5 w-4.5" />
					Exit controller
				</button>
			</div>
		</aside>
	)
}
