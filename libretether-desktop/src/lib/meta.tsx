// Single source of truth for the per-controller-type and per-OS presentation
// metadata (label, icon, help/run-hint text), so the launch screen, the
// controller form, the sidebar, the deploy modal and the machine list don't each
// re-declare (and risk drifting on) the same maps.

import { Apple, type LucideIcon, Monitor, Network, Server, Terminal, Wifi } from "lucide-react"
import type { ClientOs, ControllerType } from "./types"

export const CONTROLLER_TYPE_META: Record<ControllerType, { label: string; icon: LucideIcon; help: string }> = {
	direct: {
		help: "Agents dial this machine directly — over your LAN, an existing VPN, or a port-forward. You provide the address they should reach.",
		icon: Network,
		label: "Direct"
	},
	relay: {
		help: "This controller and every agent dial out to a libretether-relay you run on a public host. Nothing on either end needs to be exposed.",
		icon: Server,
		label: "Relay"
	},
	tailscale: {
		help: "Agents join your tailnet with a pre-auth key, then dial this machine's tailnet address. No ports to expose.",
		icon: Wifi,
		label: "Tailscale"
	}
}

export const OS_META: Record<ClientOs, { label: string; icon: LucideIcon; runHint: string }> = {
	linux: { icon: Terminal, label: "Linux", runHint: "Run this on the client machine you want to control:" },
	macos: { icon: Apple, label: "macOS", runHint: "Run this on the client Mac you want to control:" },
	windows: {
		icon: Monitor,
		label: "Windows",
		runHint: "Run this in PowerShell on the client PC you want to control:"
	}
}
