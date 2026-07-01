import { Monitor } from "lucide-react"
import { OS_META } from "../lib/meta"
import type { ClientOs } from "../lib/types"

export function OsIcon({ os, className }: { os: ClientOs; className?: string }) {
	const Icon = OS_META[os]?.icon ?? Monitor

	return <Icon className={className} />
}

export function osLabel(os: ClientOs): string {
	return OS_META[os]?.label ?? os
}
