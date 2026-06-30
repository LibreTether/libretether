import { Plus, Rocket, Smartphone, Terminal } from "lucide-react"
import { useEffect, useState } from "react"
import * as api from "../lib/api"
import { cn } from "../lib/cn"
import { OS_META } from "../lib/meta"
import type { ActiveInfo, ClientOs, PairingStarted } from "../lib/types"
import { useAsyncAction } from "../lib/useAsyncAction"
import { Combobox } from "./Combobox"
import { DeployScript } from "./DeployScript"
import { OsIcon } from "./OsIcon"
import { PhoneInstall } from "./PhoneInstall"
import { Button, Drawer, Field, Input } from "./ui"

const OS_OPTIONS = (Object.keys(OS_META) as ClientOs[]).map((os) => ({
	icon: <OsIcon className="h-4 w-4" os={os} />,
	label: OS_META[os].label,
	value: os
}))

interface Created {
	name: string
	os: ClientOs
	script: string
}

/** How a new machine is brought online: paste a one-line command, or pair it from a
 *  phone-friendly browser flow. Pairing needs a relay (the new machine reaches the
 *  controller through it), so it's only offered for relay controllers. */
type Mode = "command" | "phone"

/** Add a machine in one surface: name + OS, then either the deploy command or the
 *  phone-pairing instructions appear inline in the same drawer. */
export function AddMachineDrawer({
	open,
	onClose,
	onCreated,
	active
}: {
	open: boolean
	onClose: () => void
	onCreated: () => void
	active: ActiveInfo
}) {
	const action = useAsyncAction()
	const [name, setName] = useState("")
	const [os, setOs] = useState<ClientOs>("linux")
	const [mode, setMode] = useState<Mode>("command")
	const [created, setCreated] = useState<Created | null>(null)
	const [pairing, setPairing] = useState<PairingStarted | null>(null)

	const canPair = active.kind.type === "relay"

	// Reset to a blank form every time the drawer opens.
	useEffect(() => {
		if (open) {
			setName("")
			setOs("linux")
			setMode("command")
			setCreated(null)
			setPairing(null)
		}
	}, [open])

	const submit = async () => {
		if (!name.trim()) return
		if (mode === "phone") {
			await action.run("Couldn't start pairing", async () => {
				setPairing(await api.openPairing(name.trim(), os))
				onCreated()
			})
		} else {
			await action.run("Couldn't create machine", async () => {
				const res = await api.createClient(name.trim(), os)
				setCreated({ name: res.client.name, os: res.client.os, script: res.deploy_script })
				onCreated()
			})
		}
	}

	const done = created || pairing
	const title = pairing ? `Pair ${pairing.client.name}` : created ? `Deploy ${created.name}` : "Add a machine"
	const subtitle = pairing
		? "Read the steps out to the person at the computer"
		: created
			? "Run the command on the target machine"
			: "Name it and pick its operating system"

	return (
		<Drawer
			footer={
				done ? (
					<Button onClick={onClose} variant="outline">
						Done
					</Button>
				) : (
					<>
						<Button onClick={onClose} variant="ghost">
							Cancel
						</Button>
						<Button
							icon={
								mode === "phone" ? <Smartphone className="h-4 w-4" /> : <Rocket className="h-4 w-4" />
							}
							loading={action.busy}
							onClick={submit}
							variant="primary"
						>
							{mode === "phone" ? "Create & start pairing" : "Create & get command"}
						</Button>
					</>
				)
			}
			icon={done ? <Rocket className="h-5 w-5" /> : <Plus className="h-5 w-5" />}
			onClose={onClose}
			open={open}
			size="md"
			subtitle={subtitle}
			title={title}
		>
			{pairing ? (
				<PhoneInstall started={pairing} />
			) : created ? (
				<DeployScript name={created.name} os={created.os} script={created.script} />
			) : (
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
					<Field hint="Picks the right deploy command for the target." label="Operating system">
						<Combobox<ClientOs> onChange={setOs} options={OS_OPTIONS} value={os} />
					</Field>

					{canPair && (
						<Field
							hint={
								mode === "phone"
									? "They open a site, type a short code, and run a one-click installer — nothing to paste."
									: "Run a one-line command on the machine yourself."
							}
							label="How will you set it up?"
						>
							<div className="grid grid-cols-2 gap-1.5 rounded-xl border border-border bg-surface-2 p-1">
								<ModeTab
									active={mode === "command"}
									icon={<Terminal className="h-4 w-4" />}
									label="Paste a command"
									onClick={() => setMode("command")}
								/>
								<ModeTab
									active={mode === "phone"}
									icon={<Smartphone className="h-4 w-4" />}
									label="Phone install"
									onClick={() => setMode("phone")}
								/>
							</div>
						</Field>
					)}
				</div>
			)}
		</Drawer>
	)
}

function ModeTab({
	active,
	icon,
	label,
	onClick
}: {
	active: boolean
	icon: React.ReactNode
	label: string
	onClick: () => void
}) {
	return (
		<button
			className={cn(
				"flex items-center justify-center gap-2 rounded-lg px-3 py-2 text-sm font-semibold transition",
				active ? "bg-surface-3 text-text shadow-sm" : "text-muted hover:text-text"
			)}
			onClick={onClick}
			type="button"
		>
			{icon}
			{label}
		</button>
	)
}
