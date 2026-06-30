import { useState } from "react"
import { useConfirm } from "../components/confirm"
import * as api from "./api"
import { useToast } from "./toast"
import type { ClientDto, ClientOs } from "./types"
import { useAsyncAction } from "./useAsyncAction"

export interface DeployState {
	name: string
	os: ClientOs
	script: string
}

/** The async machine operations (SSH, RDP, remove, deploy/re-deploy) shared by the
 *  machine list and the command palette, so both drive identical behaviour and the
 *  same per-action in-flight guard. Control and Details are pure UI state and stay
 *  in the shell.
 *
 *  `pending` is keyed `${op}:${id}` so each button on each row disables only itself
 *  while its own call is in flight — RDP/SSH do a multi-second reachability probe,
 *  so a shared flag would either freeze every row or allow double-fires. */
export function useMachineActions(reload: () => void, onDeploy: (d: DeployState) => void) {
	const toast = useToast()
	const confirm = useConfirm()
	const action = useAsyncAction()
	const [pending, setPending] = useState<Record<string, boolean>>({})

	const runExclusive = async (key: string, fn: () => Promise<unknown>) => {
		if (pending[key]) return
		setPending((p) => ({ ...p, [key]: true }))
		try {
			await fn()
		} finally {
			setPending((p) => ({ ...p, [key]: false }))
		}
	}

	const ssh = (client: ClientDto) =>
		runExclusive(`ssh:${client.id}`, () =>
			action.run("SSH failed", async () => {
				await api.connectSsh(client.id)
				toast.info("Opening SSH", `Launching a terminal to ${client.name}…`)
			})
		)

	const rdp = (client: ClientDto) =>
		runExclusive(`rdp:${client.id}`, async () => {
			// On Windows (workstation editions) an incoming RDP connection takes over
			// the machine: whoever is signed in at the physical screen is disconnected
			// and the console locks. Warn before doing that to someone's live session.
			if (client.os === "windows") {
				const ok = await confirm({
					confirmLabel: "Open RDP anyway",
					message: `Opening an RDP session will disconnect whoever is signed in at ${client.name}'s screen right now — their session is taken over and the local screen locks. Continue?`,
					title: "RDP disconnects the current Windows user",
					tone: "danger"
				})
				if (!ok) return
			}
			await action.run("RDP failed", async () => {
				await api.connectRdp(client.id)
				toast.info("Launching RDP", `Opening an RDP session to ${client.name}…`)
			})
		})

	const remove = (client: ClientDto) =>
		runExclusive(`remove:${client.id}`, async () => {
			const ok = await confirm({
				confirmLabel: "Remove",
				message: `Remove ${client.name}? The agent on that machine will no longer be able to connect.`,
				title: "Remove machine",
				tone: "danger"
			})
			if (!ok) return
			await action.run("Couldn't remove machine", async () => {
				await api.removeClient(client.id)
				reload()
			})
		})

	const deploy = (client: ClientDto) =>
		runExclusive(`deploy:${client.id}`, async () => {
			// Decide from the client's own state — not by catching a failure — so an
			// unrelated error (offline, backend hiccup) can't masquerade as "enrolled"
			// and drop the user into the destructive reset dialog.
			if (!client.enrolled) {
				await action.run("Couldn't get the deploy script", async () => {
					const script = await api.getDeployScript(client.id, client.os)
					onDeploy({ name: client.name, os: client.os, script })
				})
				return
			}
			// Already enrolled — resetting issues a new one-time token and revokes the
			// old agent key, so confirm first.
			const ok = await confirm({
				confirmLabel: "Reset & re-deploy",
				message: `${client.name} is already enrolled. Resetting issues a new one-time token and revokes the old agent key. Continue?`,
				title: "Re-deploy machine",
				tone: "danger"
			})
			if (!ok) return
			await action.run("Couldn't reset token", async () => {
				const res = await api.resetToken(client.id)
				onDeploy({ name: res.client.name, os: res.client.os, script: res.deploy_script })
				reload()
			})
		})

	return { deploy, pending, rdp, remove, ssh }
}
