import { writeText } from "@tauri-apps/plugin-clipboard-manager"
import { save } from "@tauri-apps/plugin-dialog"
import { Check, Copy, Download, Terminal } from "lucide-react"
import { useState } from "react"
import * as api from "../lib/api"
import { slug } from "../lib/format"
import { useToast } from "../lib/toast"
import type { ClientOs } from "../lib/types"
import { Button, Modal } from "./ui"

const RUN_HINT: Record<ClientOs, string> = {
	linux: "On the client machine, save this script and run:  bash tether-deploy.sh",
	macos: "On the client Mac, save this script and run:  bash tether-deploy.sh",
	windows: "On the client PC, save this script and run it in PowerShell:  .\\tether-deploy.ps1"
}

export function DeployModal({
	open,
	onClose,
	name,
	os,
	script
}: {
	open: boolean
	onClose: () => void
	name: string
	os: ClientOs
	script: string
}) {
	const toast = useToast()
	const [copied, setCopied] = useState(false)

	const copy = async () => {
		try {
			await writeText(script)
			setCopied(true)
			setTimeout(() => setCopied(false), 1500)
		} catch (e) {
			toast.error("Copy failed", api.errString(e))
		}
	}

	const download = async () => {
		const ext = os === "windows" ? "ps1" : "sh"
		try {
			const path = await save({ defaultPath: `tether-deploy-${slug(name)}.${ext}` })
			if (!path) return
			await api.saveTextFile(path, script)
			toast.success("Script saved", path)
		} catch (e) {
			toast.error("Save failed", api.errString(e))
		}
	}

	return (
		<Modal
			footer={
				<>
					<Button onClick={onClose} variant="ghost">
						Done
					</Button>
					<Button icon={<Download className="h-4 w-4" />} onClick={download} variant="outline">
						Save script…
					</Button>
					<Button
						icon={copied ? <Check className="h-4 w-4" /> : <Copy className="h-4 w-4" />}
						onClick={copy}
						variant="primary"
					>
						{copied ? "Copied" : "Copy"}
					</Button>
				</>
			}
			onClose={onClose}
			open={open}
			size="lg"
			title={`Deploy ${name}`}
		>
			<div className="flex flex-col gap-3">
				<div className="flex items-start gap-2.5 rounded-xl bg-primary-soft/60 px-3.5 py-3 text-sm text-muted">
					<Terminal className="mt-0.5 h-4 w-4 shrink-0 text-primary dark:text-primary-strong" />
					<div>
						<p className="font-medium text-text">{RUN_HINT[os]}</p>
						<p className="mt-1 text-xs">
							The script connects the machine to your controller (via Tailscale if you set an auth key on
							the Controller page, otherwise directly), installs the background agent, and enrols it. It
							runs once and keeps the agent running on every boot.
						</p>
					</div>
				</div>

				<pre className="max-h-[44vh] overflow-auto rounded-xl border border-border bg-surface-2 p-3.5 text-[0.78rem] leading-relaxed text-text">
					<code>{script}</code>
				</pre>

				<p className="text-xs text-subtle">
					Before running, point the script at the agent binary on the client: set
					<code className="mx-1 rounded bg-surface-3 px-1 py-0.5">TETHER_AGENT_BIN</code>
					to a local path or
					<code className="mx-1 rounded bg-surface-3 px-1 py-0.5">TETHER_AGENT_URL</code>
					to a download URL.
				</p>
			</div>
		</Modal>
	)
}
