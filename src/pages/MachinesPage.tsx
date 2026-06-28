import {
	MonitorSmartphone,
	MonitorUp,
	Plus,
	Rocket,
	ScreenShare,
	SlidersHorizontal,
	Terminal,
	Trash2
} from "lucide-react"
import { useState } from "react"
import { ClientDetailModal } from "../components/ClientDetailModal"
import { useConfirm } from "../components/confirm"
import { DeployModal } from "../components/DeployModal"
import { OsIcon, osLabel } from "../components/OsIcon"
import { Badge, Button, EmptyState, Field, Input, Modal, Select } from "../components/ui"
import * as api from "../lib/api"
import { relativeTime } from "../lib/format"
import { useToast } from "../lib/toast"
import type { ClientDto, ClientOs } from "../lib/types"

interface DeployState {
	name: string
	os: ClientOs
	script: string
}

export function MachinesPage({
	clients,
	loading,
	onControl,
	onReload
}: {
	clients: ClientDto[]
	loading: boolean
	onControl: (c: ClientDto) => void
	onReload: () => void
}) {
	const toast = useToast()
	const confirm = useConfirm()
	const [createOpen, setCreateOpen] = useState(false)
	const [deploy, setDeploy] = useState<DeployState | null>(null)
	const [detail, setDetail] = useState<ClientDto | null>(null)

	const openDeploy = async (client: ClientDto) => {
		try {
			const script = await api.getDeployScript(client.id, client.os)
			setDeploy({ name: client.name, os: client.os, script })
		} catch {
			// Already enrolled — offer to reset its token and re-deploy.
			const ok = await confirm({
				confirmLabel: "Reset & re-deploy",
				message: `${client.name} is already enrolled. Resetting issues a new one-time token and revokes the old agent key. Continue?`,
				title: "Re-deploy machine",
				tone: "danger"
			})
			if (!ok) return
			try {
				const res = await api.resetToken(client.id)
				setDeploy({ name: res.client.name, os: res.client.os, script: res.deploy_script })
				onReload()
			} catch (e) {
				toast.error("Couldn't reset token", api.errString(e))
			}
		}
	}

	const remove = async (client: ClientDto) => {
		const ok = await confirm({
			confirmLabel: "Remove",
			message: `Remove ${client.name}? The agent on that machine will no longer be able to connect.`,
			title: "Remove machine",
			tone: "danger"
		})
		if (!ok) return
		try {
			await api.removeClient(client.id)
			onReload()
		} catch (e) {
			toast.error("Couldn't remove machine", api.errString(e))
		}
	}

	const rdp = async (client: ClientDto) => {
		try {
			await api.connectRdp(client.id)
			toast.info("Launching RDP", `Opening an RDP session to ${client.name}…`)
		} catch (e) {
			toast.error("RDP failed", api.errString(e))
		}
	}

	const ssh = async (client: ClientDto) => {
		try {
			await api.connectSsh(client.id)
			toast.info("Opening SSH", `Launching a terminal to ${client.name}…`)
		} catch (e) {
			toast.error("SSH failed", api.errString(e))
		}
	}

	return (
		<>
			<header className="drag flex items-center justify-between border-b border-border px-7 py-5">
				<div>
					<h1 className="text-xl font-bold text-text">Machines</h1>
					<p className="text-sm text-muted">Enrol, monitor and take control of your remote machines.</p>
				</div>
				<Button icon={<Plus className="h-4 w-4" />} onClick={() => setCreateOpen(true)} variant="primary">
					Add machine
				</Button>
			</header>

			<div className="min-h-0 flex-1 overflow-y-auto px-7 py-6">
				{loading ? (
					<p className="text-sm text-subtle">Loading…</p>
				) : clients.length === 0 ? (
					<EmptyState
						action={
							<Button
								icon={<Plus className="h-4 w-4" />}
								onClick={() => setCreateOpen(true)}
								variant="primary"
							>
								Add your first machine
							</Button>
						}
						description="Create a machine to generate a one-click deployment script. Run it on the target computer to make it remotely controllable."
						icon={<MonitorSmartphone className="h-7 w-7" />}
						title="No machines yet"
					/>
				) : (
					<div className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-3">
						{clients.map((c) => (
							<ClientCard
								client={c}
								key={c.id}
								onControl={() => onControl(c)}
								onDeploy={() => openDeploy(c)}
								onDetail={() => setDetail(c)}
								onRdp={() => rdp(c)}
								onRemove={() => remove(c)}
								onSsh={() => ssh(c)}
							/>
						))}
					</div>
				)}
			</div>

			<CreateModal
				onClose={() => setCreateOpen(false)}
				onCreated={(d) => {
					setCreateOpen(false)
					setDeploy(d)
					onReload()
				}}
				open={createOpen}
			/>
			{deploy && (
				<DeployModal
					name={deploy.name}
					onClose={() => setDeploy(null)}
					open
					os={deploy.os}
					script={deploy.script}
				/>
			)}
			{detail && <ClientDetailModal client={detail} onClose={() => setDetail(null)} open />}
		</>
	)
}

function ClientCard({
	client,
	onControl,
	onDetail,
	onDeploy,
	onRdp,
	onRemove,
	onSsh
}: {
	client: ClientDto
	onControl: () => void
	onDetail: () => void
	onDeploy: () => void
	onRdp: () => void
	onRemove: () => void
	onSsh: () => void
}) {
	const { status } = client
	return (
		<div className="card flex flex-col gap-3 p-4">
			<div className="flex items-start gap-3">
				<div className="grid h-11 w-11 shrink-0 place-items-center rounded-xl bg-surface-2 text-primary dark:text-primary-strong">
					<OsIcon className="h-5 w-5" os={client.os} />
				</div>
				<div className="min-w-0 flex-1">
					<div className="truncate font-semibold text-text" title={client.name}>
						{client.name}
					</div>
					<div className="text-xs text-subtle">{osLabel(client.os)}</div>
				</div>
				{client.online ? <Badge tone="success">online</Badge> : <Badge>offline</Badge>}
			</div>

			<div className="min-h-[1.25rem] text-xs text-muted">
				{client.online && status ? (
					<span>
						{status.host.hostname} · up {Math.floor(status.uptime_secs / 60)}m · {status.displays}{" "}
						{status.displays === 1 ? "display" : "displays"}
					</span>
				) : !client.enrolled ? (
					<span className="text-warning">awaiting enrollment — run the deploy script</span>
				) : (
					<span>last seen {relativeTime(client.last_seen)}</span>
				)}
			</div>

			<div className="flex items-center gap-1.5">
				<Button
					className="flex-1"
					disabled={!client.online}
					icon={<MonitorUp className="h-4 w-4" />}
					onClick={onControl}
					size="sm"
					variant="primary"
				>
					Control
				</Button>
				<Button
					disabled={!client.online}
					icon={<ScreenShare className="h-4 w-4" />}
					onClick={onRdp}
					size="icon-sm"
					title="Connect via RDP"
					variant="outline"
				/>
				<Button
					disabled={!client.online}
					icon={<Terminal className="h-4 w-4" />}
					onClick={onSsh}
					size="icon-sm"
					title="Connect via SSH"
					variant="outline"
				/>
				<Button
					disabled={!client.online}
					icon={<SlidersHorizontal className="h-4 w-4" />}
					onClick={onDetail}
					size="icon-sm"
					title="Details"
					variant="outline"
				/>
				<Button
					icon={<Rocket className="h-4 w-4" />}
					onClick={onDeploy}
					size="icon-sm"
					title="Deploy script"
					variant="ghost"
				/>
				<Button
					icon={<Trash2 className="h-4 w-4" />}
					onClick={onRemove}
					size="icon-sm"
					title="Remove"
					variant="ghost"
				/>
			</div>
		</div>
	)
}

function CreateModal({
	open,
	onClose,
	onCreated
}: {
	open: boolean
	onClose: () => void
	onCreated: (d: DeployState) => void
}) {
	const toast = useToast()
	const [name, setName] = useState("")
	const [os, setOs] = useState<ClientOs>("linux")
	const [busy, setBusy] = useState(false)

	const submit = async () => {
		if (!name.trim()) return
		setBusy(true)
		try {
			const res = await api.createClient(name.trim(), os)
			onCreated({ name: res.client.name, os: res.client.os, script: res.deploy_script })
			setName("")
			setOs("linux")
		} catch (e) {
			toast.error("Couldn't create machine", api.errString(e))
		} finally {
			setBusy(false)
		}
	}

	return (
		<Modal
			footer={
				<>
					<Button onClick={onClose} variant="ghost">
						Cancel
					</Button>
					<Button icon={<Rocket className="h-4 w-4" />} loading={busy} onClick={submit} variant="primary">
						Create & get script
					</Button>
				</>
			}
			onClose={onClose}
			open={open}
			size="sm"
			title="Add a machine"
		>
			<div className="flex flex-col gap-4">
				<Field hint="A friendly name to recognise this machine." label="Name">
					<Input
						autoFocus
						onChange={(e) => setName(e.target.value)}
						onKeyDown={(e) => e.key === "Enter" && submit()}
						placeholder="e.g. Office iMac"
						value={name}
					/>
				</Field>
				<Field hint="Picks the right deploy script for the target." label="Operating system">
					<Select onChange={(e) => setOs(e.target.value as ClientOs)} value={os}>
						<option value="linux">Linux</option>
						<option value="macos">macOS</option>
						<option value="windows">Windows</option>
					</Select>
				</Field>
			</div>
		</Modal>
	)
}
