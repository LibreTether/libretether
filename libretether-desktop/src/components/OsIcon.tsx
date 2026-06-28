import { Apple, type LucideIcon, Monitor, Terminal } from "lucide-react"
import type { ClientOs } from "../lib/types"

const ICONS: Record<ClientOs, LucideIcon> = {
	linux: Terminal,
	macos: Apple,
	windows: Monitor
}

const LABELS: Record<ClientOs, string> = {
	linux: "Linux",
	macos: "macOS",
	windows: "Windows"
}

export function OsIcon({ os, className }: { os: ClientOs; className?: string }) {
	const Icon = ICONS[os] ?? Monitor
	return <Icon className={className} />
}

export function osLabel(os: ClientOs): string {
	return LABELS[os] ?? os
}
