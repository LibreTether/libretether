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
	linux: "Run this on the client machine you want to control:",
	macos: "Run this on the client Mac you want to control:",
	windows: "Run this in PowerShell on the client PC you want to control:"
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
		// The shebang is only needed in the saved file; the textbox/clipboard keep the bare command.
		const contents = os === "windows" ? `${script}\n` : `#!/usr/bin/env sh\n${script}\n`
		try {
			const path = await save({ defaultPath: `libretether-deploy-${slug(name)}.${ext}` })
			if (!path) return
			await api.saveTextFile(path, contents)
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
							It installs the LibreTether agent, enrols it with your controller, and keeps it running on
							every boot. Runs once — no need to save it first.
						</p>
					</div>
				</div>

				<pre className="max-h-[44vh] overflow-y-auto whitespace-pre-wrap break-all rounded-xl border border-border bg-surface-2 p-3.5 text-[0.78rem] leading-relaxed text-text">
					<code>{script}</code>
				</pre>
			</div>
		</Modal>
	)
}
