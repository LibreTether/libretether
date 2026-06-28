import type { ReactNode } from "react"
import { cn } from "../lib/cn"

export function PageHeader({
	title,
	subtitle,
	actions,
	className
}: {
	title: ReactNode
	subtitle?: ReactNode
	actions?: ReactNode
	className?: string
}) {
	return (
		<header
			className={cn(
				"drag flex items-center justify-between gap-4 border-b border-border bg-surface/30 px-7 py-4",
				className
			)}
		>
			<div className="min-w-0">
				<h1 className="truncate text-lg font-bold tracking-tight text-text">{title}</h1>
				{subtitle && <p className="mt-0.5 truncate text-sm text-muted">{subtitle}</p>}
			</div>
			{actions && <div className="no-drag flex items-center gap-2">{actions}</div>}
		</header>
	)
}
