import { Network, Server, Wifi } from "lucide-react"
import { useState } from "react"
import * as api from "../lib/api"
import { useToast } from "../lib/toast"
import type { ControllerKind, ControllerSummary, ControllerType } from "../lib/types"
import { useAsyncAction } from "../lib/useAsyncAction"
import { Combobox } from "./Combobox"
import { Button, Field, Input, Modal } from "./ui"

const DEFAULT_PORT = 47600

const TYPE_HELP: Record<ControllerType, string> = {
	direct: "Agents dial this machine directly — over your LAN, an existing VPN, or a port-forward. You provide the address they should reach.",
	relay: "This controller and every agent dial out to a libretether-relay you run on a public host. Nothing on either end needs to be exposed.",
	tailscale:
		"Agents join your tailnet with a pre-auth key, then dial this machine's tailnet address. No ports to expose."
}

export function ControllerForm({
	existing,
	onClose,
	onSaved
}: {
	existing: ControllerSummary | null
	onClose: () => void
	onSaved: () => void
}) {
	const toast = useToast()
	const saveAction = useAsyncAction()
	const k = existing?.kind
	const [name, setName] = useState(existing?.name ?? "")
	const [type, setType] = useState<ControllerType>(k?.type ?? "tailscale")
	const [advertise, setAdvertise] = useState(k?.type === "direct" ? (k.advertise_addr ?? "") : "")
	const [authKey, setAuthKey] = useState(k?.type === "tailscale" ? (k.auth_key ?? "") : "")
	const [port, setPort] = useState(
		String(k && (k.type === "direct" || k.type === "tailscale") ? k.listen_port : DEFAULT_PORT)
	)
	const [relayAddr, setRelayAddr] = useState(k?.type === "relay" ? k.address : "")
	const [relayOwner, setRelayOwner] = useState(k?.type === "relay" ? k.owner_secret : "")
	const [relayAgent, setRelayAgent] = useState(k?.type === "relay" ? k.agent_secret : "")

	const buildKind = (): ControllerKind => {
		const listen_port = Number.parseInt(port, 10) || DEFAULT_PORT
		if (type === "direct") return { advertise_addr: advertise.trim() || null, listen_port, type }
		if (type === "tailscale") return { auth_key: authKey.trim(), listen_port, type }
		return {
			address: relayAddr.trim(),
			agent_secret: relayAgent.trim(),
			owner_secret: relayOwner.trim(),
			type: "relay"
		}
	}

	const save = async () => {
		if (!name.trim()) {
			toast.error("Name your controller")
			return
		}
		// Direct/Tailscale bind a UDP listener, so the port must be a valid 1–65535.
		if (type !== "relay") {
			const p = Number(port)
			if (!Number.isInteger(p) || p < 1 || p > 65535) {
				toast.error("Invalid listen port", "Enter a port between 1 and 65535.")
				return
			}
		}
		if (type === "tailscale" && !authKey.trim()) {
			toast.error("Tailscale controllers require an auth key")
			return
		}
		if (type === "relay" && (!relayAddr.trim() || !relayOwner.trim() || !relayAgent.trim())) {
			toast.error("Relay needs an address, owner secret and agent secret")
			return
		}
		const ok = await saveAction.run("Couldn't save controller", async () => {
			if (existing) await api.updateController(existing.id, name.trim(), buildKind())
			else await api.createController(name.trim(), buildKind())
		})
		if (ok) onSaved()
	}

	return (
		<Modal
			footer={
				<>
					<Button onClick={onClose} variant="ghost">
						Cancel
					</Button>
					<Button loading={saveAction.busy} onClick={save} variant="primary">
						{existing ? "Save changes" : "Create controller"}
					</Button>
				</>
			}
			onClose={onClose}
			open
			size="md"
			title={existing ? "Edit controller" : "New controller"}
		>
			<div className="flex flex-col gap-4">
				<Field label="Name">
					<Input
						autoFocus
						onChange={(e) => setName(e.target.value)}
						placeholder="e.g. Home lab"
						value={name}
					/>
				</Field>

				<Field hint={TYPE_HELP[type]} label="Type">
					<Combobox<ControllerType>
						onChange={setType}
						options={[
							{ icon: <Wifi className="h-4 w-4" />, label: "Tailscale", value: "tailscale" },
							{ icon: <Network className="h-4 w-4" />, label: "Direct", value: "direct" },
							{ icon: <Server className="h-4 w-4" />, label: "Relay", value: "relay" }
						]}
						value={type}
					/>
				</Field>

				{type === "tailscale" && (
					<>
						<Field
							hint="Required. A Tailscale pre-auth key from your admin console, so agents join the tailnet without an interactive login."
							label="Auth key"
						>
							<Input
								onChange={(e) => setAuthKey(e.target.value)}
								placeholder="tskey-auth-…"
								type="password"
								value={authKey}
							/>
						</Field>
						<PortField onChange={setPort} value={port} />
					</>
				)}

				{type === "direct" && (
					<>
						<Field
							hint="host:port agents should dial — a LAN/VPN IP or a public host. Required so deploy scripts know where to connect."
							label="Advertise address"
						>
							<Input
								onChange={(e) => setAdvertise(e.target.value)}
								placeholder="e.g. 192.168.1.20:47600 or my-host.example.com:47600"
								value={advertise}
							/>
						</Field>
						<PortField onChange={setPort} value={port} />
					</>
				)}

				{type === "relay" && (
					<>
						<Field hint="host:port of your libretether-relay (its public IP/DNS)." label="Relay address">
							<Input
								onChange={(e) => setRelayAddr(e.target.value)}
								placeholder="e.g. relay.example.com:47600"
								value={relayAddr}
							/>
						</Field>
						<Field
							hint="From `libretether-relay info` — authenticates this controller as the owner."
							label="Owner secret"
						>
							<Input onChange={(e) => setRelayOwner(e.target.value)} type="password" value={relayOwner} />
						</Field>
						<Field
							hint="From `libretether-relay info` — embedded in this controller's deploy scripts."
							label="Agent secret"
						>
							<Input onChange={(e) => setRelayAgent(e.target.value)} type="password" value={relayAgent} />
						</Field>
					</>
				)}
			</div>
		</Modal>
	)
}

function PortField({ value, onChange }: { value: string; onChange: (v: string) => void }) {
	return (
		<Field hint="UDP port this controller listens on (QUIC). Default 47600." label="Listen port">
			<Input inputMode="numeric" onChange={(e) => onChange(e.target.value)} placeholder="47600" value={value} />
		</Field>
	)
}
