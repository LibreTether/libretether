import { RefreshCw } from "lucide-react"
import { useCallback, useEffect, useState } from "react"
import * as api from "../lib/api"
import { cn } from "../lib/cn"
import { formatUptime } from "../lib/format"
import { useToast } from "../lib/toast"
import type { ActiveInfo, AgentStatus, ClientDto, EncoderPref } from "../lib/types"
import { useAsyncAction } from "../lib/useAsyncAction"
import { OsIcon, osLabel } from "./OsIcon"
import { SecurityPanel } from "./SecurityPanel"
import { Button, Drawer, Modal } from "./ui"

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

const ENCODERS: { value: EncoderPref; label: string; hint: string }[] = [
	{ hint: "Let the agent pick the best encoder it can run.", label: "Auto", value: "auto" },
	{ hint: "OpenH264 — works on every machine.", label: "Software", value: "software" },
	{ hint: "Platform hardware encoder (Media Foundation on Windows).", label: "Hardware", value: "hardware" },
	{ hint: "Zero-copy GPU pipeline (Windows) — lowest CPU cost.", label: "GPU", value: "gpu" }
]

/** Choose which encoder this machine uses for live sessions. Persisted on the
 *  controller and sent to the agent at each session start (nothing on the agent).
 *  Options the agent doesn't advertise as supported are disabled; changing it while a
 *  session to this machine is live restarts that session (after a confirm). */
function EncoderSection({
	client,
	status,
	controlling,
	onRestart
}: {
	client: ClientDto
	status: AgentStatus | null
	controlling: boolean
	onRestart: () => void
}) {
	const toast = useToast()
	const save = useAsyncAction()
	const [confirm, setConfirm] = useState<EncoderPref | null>(null)
	// Software is universal; hardware/GPU only when the online agent advertises them.
	// Offline (no status) we can't know, so only Auto + Software are selectable.
	const supported = new Set<EncoderPref>(status?.encoders ?? ["software"])
	const enabled = (e: EncoderPref) => e === "auto" || supported.has(e)

	const apply = async (encoder: EncoderPref, restart: boolean) => {
		let changed = false
		const ok = await save.run("Couldn't set encoder", async () => {
			changed = await api.setClientEncoder(client.id, encoder)
		})
		if (!ok) return
		if (changed && restart) {
			onRestart()
			toast.success("Encoder changed", "Restarting the session…")
		} else {
			toast.success("Encoder saved", "Takes effect on the next session.")
		}
	}

	const pick = (encoder: EncoderPref) => {
		if (encoder === client.encoder) return
		// A live session to this machine must restart to switch pipelines — confirm.
		if (controlling) setConfirm(encoder)
		else apply(encoder, false)
	}

	return (
		<section className="flex flex-col gap-2.5">
			<div className="eyebrow">Encoder</div>
			<div className="grid grid-cols-4 gap-px overflow-hidden rounded-xl border border-border bg-border">
				{ENCODERS.map((o) => {
					const on = enabled(o.value)
					const selected = client.encoder === o.value
					return (
						<button
							className={cn(
								"px-2 py-2 text-center text-sm transition-colors",
								selected
									? "bg-primary/15 font-medium text-primary"
									: "bg-surface-2 text-text hover:bg-surface-3",
								!on && "cursor-not-allowed bg-surface-2 text-muted opacity-40 hover:bg-surface-2"
							)}
							disabled={!on || save.busy}
							key={o.value}
							onClick={() => pick(o.value)}
							title={on ? o.hint : `${o.label} isn't supported by this machine`}
							type="button"
						>
							{o.label}
						</button>
					)
				})}
			</div>
			<p className="text-xs text-subtle">
				{ENCODERS.find((e) => e.value === client.encoder)?.hint} Sent to the agent at session start; nothing is
				stored on the machine.
			</p>
			{confirm && (
				<Modal
					footer={
						<>
							<Button onClick={() => setConfirm(null)} variant="ghost">
								Cancel
							</Button>
							<Button
								loading={save.busy}
								onClick={async () => {
									const e = confirm
									setConfirm(null)
									await apply(e, true)
								}}
								variant="primary"
							>
								Restart session
							</Button>
						</>
					}
					onClose={() => setConfirm(null)}
					open
					size="sm"
					title="Restart the session?"
				>
					<p className="text-sm text-text">
						Switching to <b>{ENCODERS.find((e) => e.value === confirm)?.label}</b> restarts the live screen
						session so the new encoder pipeline can start.
					</p>
				</Modal>
			)}
		</section>
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
	active,
	controlling,
	onRestartSession
}: {
	open: boolean
	onClose: () => void
	client: ClientDto
	active: ActiveInfo
	/** True when a live screen session to this machine is open (so an encoder change
	 *  offers to restart it). */
	controlling: boolean
	/** Restart the live session to this machine (used after an encoder change). */
	onRestartSession: () => void
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

				<EncoderSection
					client={client}
					controlling={controlling}
					onRestart={onRestartSession}
					status={status}
				/>

				<SecurityPanel active={active} client={client} />
			</div>
		</Drawer>
	)
}
