import { MonitorSmartphone, type Radio, Settings } from "lucide-react"
import { cn } from "../lib/cn"

export type Page = "machines" | "controller"

const NAV: { id: Page; label: string; icon: typeof Radio }[] = [
	{ icon: MonitorSmartphone, id: "machines", label: "Machines" },
	{ icon: Settings, id: "controller", label: "Controller" }
]

export function Sidebar({
	page,
	onNavigate,
	onlineCount
}: {
	page: Page
	onNavigate: (p: Page) => void
	onlineCount: number
}) {
	return (
		<aside className="flex w-60 shrink-0 flex-col border-r border-border bg-surface/40">
			<div className="drag flex items-center gap-2.5 px-5 pb-4 pt-6">
				<img alt="" className="h-8 w-8 rounded-lg" src="/libretether.svg" />
				<div className="leading-tight">
					<div className="text-sm font-bold text-text">LibreTether</div>
					<div className="text-[0.7rem] text-subtle">remote control</div>
				</div>
			</div>

			<nav className="flex flex-col gap-1 px-3 py-2">
				{NAV.map((item) => {
					const active = page === item.id
					const Icon = item.icon
					return (
						<button
							className={cn(
								"no-drag flex items-center gap-3 rounded-xl px-3 py-2.5 text-sm font-medium transition",
								active
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

			<div className="mt-auto px-5 pb-5">
				<div className="flex items-center gap-2 rounded-xl bg-surface-2 px-3 py-2.5 text-xs">
					<span className={cn("h-2 w-2 rounded-full", onlineCount > 0 ? "bg-success" : "bg-subtle")} />
					<span className="text-muted">
						{onlineCount} {onlineCount === 1 ? "machine" : "machines"} online
					</span>
				</div>
			</div>
		</aside>
	)
}
