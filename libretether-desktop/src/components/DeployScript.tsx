import { writeText } from "@tauri-apps/plugin-clipboard-manager"
import { save } from "@tauri-apps/plugin-dialog"
import { Check, Copy, Download, Terminal } from "lucide-react"
import { useState } from "react"
import * as api from "../lib/api"
import { slug } from "../lib/format"
import { OS_META } from "../lib/meta"
import { useToast } from "../lib/toast"
import type { ClientOs } from "../lib/types"
import { useAsyncAction } from "../lib/useAsyncAction"
import { Button } from "./ui"

/** The deploy-command view: a one-line install command to run on the target
 *  machine, with copy + save. Presentational — it carries no dialog of its own, so
 *  the add-machine drawer can show it inline and the re-deploy drawer can reuse it. */
export function DeployScript({ name, os, script }: { name: string; os: ClientOs; script: string }) {
	const toast = useToast()
	const saveAction = useAsyncAction()
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

	const download = () => {
		const ext = os === "windows" ? "ps1" : "sh"
		// The shebang is only needed in the saved file; the textbox/clipboard keep the bare command.
		const contents = os === "windows" ? `${script}\n` : `#!/usr/bin/env sh\n${script}\n`
		return saveAction.run("Save failed", async () => {
			const path = await save({ defaultPath: `libretether-deploy-${slug(name)}.${ext}` })
			if (!path) return
			await api.saveTextFile(path, contents)
			toast.success("Script saved", path)
		})
	}

	return (
		<div className="flex flex-col gap-3">
			<div className="flex items-start gap-2.5 rounded-xl border border-primary-soft bg-primary-soft/50 px-3.5 py-3 text-sm text-muted">
				<Terminal className="mt-0.5 h-4 w-4 shrink-0 text-primary dark:text-primary-strong" />
				<div>
					<p className="font-medium text-text">{OS_META[os].runHint}</p>
					<p className="mt-1 text-xs leading-relaxed">
						It installs the agent, enrols it with this controller, and keeps it running on every boot. Runs
						once — no need to save it first. The machine appears online here within seconds.
					</p>
				</div>
			</div>

			<pre className="max-h-[42vh] select-text overflow-y-auto whitespace-pre-wrap break-all rounded-xl border border-border bg-surface-2 p-3.5 text-[0.78rem] leading-relaxed text-text">
				<code>{script}</code>
			</pre>

			<div className="flex items-center justify-end gap-2.5">
				<Button
					icon={<Download className="h-4 w-4" />}
					loading={saveAction.busy}
					onClick={download}
					variant="outline"
				>
					Save script…
				</Button>
				<Button
					className="min-w-28"
					icon={copied ? <Check className="h-4 w-4" /> : <Copy className="h-4 w-4" />}
					onClick={copy}
					variant="primary"
				>
					{copied ? "Copied" : "Copy command"}
				</Button>
			</div>
		</div>
	)
}
