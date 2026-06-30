import { Plus, Rocket } from "lucide-react"
import { useEffect, useState } from "react"
import * as api from "../lib/api"
import { OS_META } from "../lib/meta"
import type { ClientOs } from "../lib/types"
import { useAsyncAction } from "../lib/useAsyncAction"
import { Combobox } from "./Combobox"
import { DeployScript } from "./DeployScript"
import { OsIcon } from "./OsIcon"
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

/** Add a machine in one surface: name + OS, then the deploy command appears inline
 *  in the same drawer — no second dialog to chase. */
export function AddMachineDrawer({
	open,
	onClose,
	onCreated
}: {
	open: boolean
	onClose: () => void
	onCreated: () => void
}) {
	const createAction = useAsyncAction()
	const [name, setName] = useState("")
	const [os, setOs] = useState<ClientOs>("linux")
	const [created, setCreated] = useState<Created | null>(null)

	// Reset to a blank form every time the drawer opens.
	useEffect(() => {
		if (open) {
			setName("")
			setOs("linux")
			setCreated(null)
		}
	}, [open])

	const submit = async () => {
		if (!name.trim()) return
		await createAction.run("Couldn't create machine", async () => {
			const res = await api.createClient(name.trim(), os)
			setCreated({ name: res.client.name, os: res.client.os, script: res.deploy_script })
			onCreated()
		})
	}

	return (
		<Drawer
			footer={
				created ? (
					<Button onClick={onClose} variant="outline">
						Done
					</Button>
				) : (
					<>
						<Button onClick={onClose} variant="ghost">
							Cancel
						</Button>
						<Button
							icon={<Rocket className="h-4 w-4" />}
							loading={createAction.busy}
							onClick={submit}
							variant="primary"
						>
							Create & get command
						</Button>
					</>
				)
			}
			icon={created ? <Rocket className="h-5 w-5" /> : <Plus className="h-5 w-5" />}
			onClose={onClose}
			open={open}
			size="md"
			subtitle={created ? "Run the command on the target machine" : "Name it and pick its operating system"}
			title={created ? `Deploy ${created.name}` : "Add a machine"}
		>
			{created ? (
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
				</div>
			)}
		</Drawer>
	)
}
