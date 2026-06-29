import { LogOut, MonitorSmartphone, Plug, type Radio } from "lucide-react"
import { cn } from "../lib/cn"
import { CONTROLLER_TYPE_META } from "../lib/meta"
import type { ActiveInfo } from "../lib/types"

export type Page = "machines" | "controller"

const NAV: { id: Page; label: string; icon: typeof Radio }[] = [
	{ icon: MonitorSmartphone, id: "machines", label: "Machines" },
	{ icon: Plug, id: "controller", label: "Connection" }
]

export function Sidebar({
	active,
	page,
	onNavigate,
	onlineCount,
	onExit
}: {
	active: ActiveInfo
	page: Page
	onNavigate: (p: Page) => void
	onlineCount: number
	onExit: () => void
}) {
	return (
		<aside className="flex w-60 shrink-0 flex-col border-r border-border bg-surface/40">
			<div className="drag flex items-center gap-2.5 px-5 pb-4 pt-6">
				<img alt="" className="h-8 w-8 rounded-lg" src="/libretether.png" />
				<div className="min-w-0 leading-tight">
					<div className="truncate text-sm font-bold text-text">{active.name}</div>
					<div className="text-[0.7rem] text-subtle">
						{CONTROLLER_TYPE_META[active.kind.type].label} controller
					</div>
				</div>
			</div>

			<nav className="flex flex-col gap-1 px-3 py-2">
				{NAV.map((item) => {
					const isActive = page === item.id
					const Icon = item.icon
					return (
						<button
							className={cn(
								"no-drag flex items-center gap-3 rounded-xl px-3 py-2.5 text-sm font-medium transition",
								isActive
									? "bg-primary-soft text-primary dark:text-primary-strong"
									: "text-muted hover:bg-surface-3 hover:text-text"
							)}
							key={item.id}
							onClick={() => onNavigate(item.id)}
						>
							<Icon className="h-4.5 w-4.5" />
							{item.label}
						</button>
					)
				})}
			</nav>

			<div className="mt-auto flex flex-col gap-2 px-3 pb-4">
				<div className="mx-2 flex items-center gap-2 rounded-xl bg-surface-2 px-3 py-2.5 text-xs">
					<span className={cn("h-2 w-2 rounded-full", onlineCount > 0 ? "bg-success" : "bg-subtle")} />
					<span className="text-muted">
						{onlineCount} {onlineCount === 1 ? "machine" : "machines"} online
					</span>
				</div>
				<button
					className="no-drag flex items-center gap-3 rounded-xl px-3 py-2.5 text-sm font-medium text-muted transition hover:bg-danger-soft hover:text-danger"
					onClick={onExit}
				>
					<LogOut className="h-4.5 w-4.5" />
					Exit controller
				</button>
			</div>
		</aside>
	)
}
