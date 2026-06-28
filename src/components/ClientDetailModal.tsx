import { Camera, Play, RefreshCw } from "lucide-react"
import { useCallback, useEffect, useState } from "react"
import * as api from "../lib/api"
import { formatUptime } from "../lib/format"
import { useToast } from "../lib/toast"
import type { AgentStatus, ClientDto, ExecResult } from "../lib/types"
import { Button, Field, Input, Modal } from "./ui"

function StatusGrid({ status }: { status: AgentStatus }) {
	const rows: [string, string][] = [
		["Hostname", status.host.hostname],
		["User", status.host.username],
		["OS", status.host.os],
		["Arch", status.host.arch],
		["Agent", `v${status.agent_version}`],
		["Uptime", formatUptime(status.uptime_secs)],
		["Displays", String(status.displays)]
	]
	return (
		<div className="grid grid-cols-2 gap-x-4 gap-y-2 rounded-xl border border-border bg-surface-2 p-3.5 text-sm sm:grid-cols-3">
			{rows.map(([k, v]) => (
				<div key={k}>
					<div className="text-[0.7rem] font-semibold uppercase tracking-wide text-subtle">{k}</div>
					<div className="truncate text-text" title={v}>
						{v}
					</div>
				</div>
			))}
		</div>
	)
}

export function ClientDetailModal({
	open,
	onClose,
	client
}: {
	open: boolean
	onClose: () => void
	client: ClientDto
}) {
	const toast = useToast()
	const [status, setStatus] = useState<AgentStatus | null>(client.status)
	const [busy, setBusy] = useState(false)
	const [cmd, setCmd] = useState("")
	const [exec, setExec] = useState<ExecResult | null>(null)
	const [execBusy, setExecBusy] = useState(false)
	const [shot, setShot] = useState<string | null>(null)
	const [shotBusy, setShotBusy] = useState(false)

	const refresh = useCallback(() => {
		setBusy(true)
		api.clientStatus(client.id)
			.then(setStatus)
			.catch((e) => toast.error("Status failed", api.errString(e)))
			.finally(() => setBusy(false))
	}, [client.id, toast])

	useEffect(() => {
		if (open && client.online) refresh()
	}, [open, client.online, refresh])

	const run = () => {
		const parts = cmd.trim().split(/\s+/).filter(Boolean)
		if (parts.length === 0) return
		setExecBusy(true)
		setExec(null)
		api.clientExec(client.id, parts[0], parts.slice(1))
			.then(setExec)
			.catch((e) => toast.error("Command failed", api.errString(e)))
			.finally(() => setExecBusy(false))
	}

	const screenshot = () => {
		setShotBusy(true)
		api.clientScreenshot(client.id)
			.then((s) => setShot(`data:image/png;base64,${s.png_base64}`))
			.catch((e) => toast.error("Screenshot failed", api.errString(e)))
			.finally(() => setShotBusy(false))
	}

	return (
		<Modal onClose={onClose} open={open} size="lg" title={client.name}>
			{!client.online ? (
				<p className="py-6 text-center text-sm text-muted">
					This machine is offline. Connect it to inspect it.
				</p>
			) : (
				<div className="flex flex-col gap-5">
					<section className="flex flex-col gap-2">
						<div className="flex items-center justify-between">
							<h3 className="text-sm font-semibold text-text">Status</h3>
							<Button
								icon={<RefreshCw className="h-3.5 w-3.5" />}
								loading={busy}
								onClick={refresh}
								size="sm"
								variant="ghost"
							>
								Refresh
							</Button>
						</div>
						{status ? <StatusGrid status={status} /> : <p className="text-sm text-subtle">Loading…</p>}
					</section>

					<section className="flex flex-col gap-2">
						<h3 className="text-sm font-semibold text-text">Run a command</h3>
						<div className="flex gap-2">
							<Field className="flex-1">
								<Input
									onChange={(e) => setCmd(e.target.value)}
									onKeyDown={(e) => e.key === "Enter" && run()}
									placeholder="e.g. uname -a"
									value={cmd}
								/>
							</Field>
							<Button
								icon={<Play className="h-4 w-4" />}
								loading={execBusy}
								onClick={run}
								variant="solid"
							>
								Run
							</Button>
						</div>
						{exec && (
							<div className="rounded-xl border border-border bg-surface-2 p-3 text-xs">
								<div className="mb-1.5 text-subtle">
									exit code {exec.code ?? "—"} · {exec.duration_ms}ms
								</div>
								{exec.stdout && (
									<pre className="overflow-auto whitespace-pre-wrap text-text">{exec.stdout}</pre>
								)}
								{exec.stderr && (
									<pre className="overflow-auto whitespace-pre-wrap text-danger">{exec.stderr}</pre>
								)}
							</div>
						)}
					</section>

					<section className="flex flex-col gap-2">
						<div className="flex items-center justify-between">
							<h3 className="text-sm font-semibold text-text">Screenshot</h3>
							<Button
								icon={<Camera className="h-3.5 w-3.5" />}
								loading={shotBusy}
								onClick={screenshot}
								size="sm"
								variant="ghost"
							>
								Capture
							</Button>
						</div>
						{shot && (
							<img alt="remote screen" className="w-full rounded-xl border border-border" src={shot} />
						)}
					</section>
				</div>
			)}
		</Modal>
	)
}
