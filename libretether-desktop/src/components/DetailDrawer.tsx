import { RefreshCw } from "lucide-react"
import { useCallback, useEffect, useState } from "react"
import * as api from "../lib/api"
import { formatUptime } from "../lib/format"
import type { ActiveInfo, AgentStatus, ClientDto } from "../lib/types"
import { useAsyncAction } from "../lib/useAsyncAction"
import { OsIcon, osLabel } from "./OsIcon"
import { SecurityPanel } from "./SecurityPanel"
import { Button, Drawer } from "./ui"

function StatusGrid({ status, showTailscale }: { status: AgentStatus; showTailscale: boolean }) {
	const rows: [string, string][] = [
		["Hostname", status.host.hostname],
		["User", status.host.username],
		["OS", status.host.os],
		["Arch", status.host.arch],
		["Agent", `v${status.agent_version}`],
		["Uptime", formatUptime(status.uptime_secs)],
		["Displays", String(status.displays)]
	]
	// Only relevant when this controller reaches agents over a tailnet.
	if (showTailscale) rows.push(["Tailscale", status.tailscale_ip ?? "—"])

	return (
		<div className="grid grid-cols-2 gap-px overflow-hidden rounded-xl border border-border bg-border sm:grid-cols-3">
			{rows.map(([k, v]) => (
				<div className="bg-surface-2 px-3 py-2.5" key={k}>
					<div className="eyebrow">{k}</div>
					<div className="mt-1 truncate font-mono text-[0.82rem] text-text" title={v}>
						{v}
					</div>
				</div>
			))}
		</div>
	)
}

/** Inspect a single machine's live status. Acting on a machine (run a command,
 *  capture a screenshot, control/SSH/RDP) lives on its row in the machine list —
 *  this drawer is just the read-out. A right-side drawer rather than a modal: Esc
 *  closes it and it doesn't bury the fleet behind a heavy scrim. */
export function DetailDrawer({
	open,
	onClose,
	client,
	active
}: {
	open: boolean
	onClose: () => void
	client: ClientDto
	active: ActiveInfo
}) {
	const showTailscale = active.kind.type === "tailscale"
	const [status, setStatus] = useState<AgentStatus | null>(client.status)
	const statusAction = useAsyncAction()

	const refresh = useCallback(() => {
		statusAction.run("Status failed", async () => setStatus(await api.clientStatus(client.id)))
	}, [client.id, statusAction])

	useEffect(() => {
		if (open && client.online) refresh()
	}, [open, client.online, refresh])

	// Re-sync to the latest pushed status (the parent passes a fresh `client` on
	// every `clients:changed`); without this the drawer shows the snapshot from when
	// it opened until the user clicks Refresh.
	useEffect(() => {
		setStatus(client.status)
	}, [client.status])

	return (
		<Drawer
			icon={<OsIcon className="h-5 w-5" os={client.os} />}
			onClose={onClose}
			open={open}
			size="md"
			subtitle={client.online ? `${osLabel(client.os)} · online` : `${osLabel(client.os)} · offline`}
			title={client.name}
		>
			<div className="flex flex-col gap-6">
				{client.online ? (
					<section className="flex flex-col gap-2.5">
						<div className="flex items-center justify-between">
							<div className="eyebrow">Status</div>
							<Button
								icon={<RefreshCw className="h-3.5 w-3.5" />}
								loading={statusAction.busy}
								onClick={refresh}
								size="sm"
								variant="ghost"
							>
								Refresh
							</Button>
						</div>
						{status ? (
							<StatusGrid showTailscale={showTailscale} status={status} />
						) : (
							<p className="text-sm text-subtle">Loading…</p>
						)}
					</section>
				) : (
					<p className="rounded-xl border border-border border-dashed bg-surface-2 px-3.5 py-3 text-center text-sm text-muted">
						Offline — connect this machine to read its live status.
					</p>
				)}

				<SecurityPanel active={active} client={client} />
			</div>
		</Drawer>
	)
}
