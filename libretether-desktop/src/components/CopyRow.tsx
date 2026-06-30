import { writeText } from "@tauri-apps/plugin-clipboard-manager"
import { Copy, Eye, EyeOff } from "lucide-react"
import { useState } from "react"
import * as api from "../lib/api"
import { useToast } from "../lib/toast"
import { Button } from "./ui"

/** A copyable value. For `secret` values the text is masked behind a reveal
 *  toggle and the copy confirmation doesn't echo the value (it would otherwise
 *  land in the toast and clipboard-history for a high-value credential). */
export function CopyRow({ value, secret = false }: { value: string; secret?: boolean }) {
	const toast = useToast()
	const [revealed, setRevealed] = useState(false)
	const masked = secret && !revealed
	const copy = () =>
		writeText(value)
			.then(() => toast.success("Copied", secret ? "Copied to clipboard." : value))
			.catch((e) => toast.error("Copy failed", api.errString(e)))
	return (
		<div className="flex items-center gap-2 rounded-xl border border-border bg-surface-2 px-3.5 py-2.5">
			<code className="flex-1 truncate font-mono text-[0.82rem] text-text">
				{masked ? "•".repeat(Math.min(value.length, 24)) : value}
			</code>
			{secret && (
				<Button
					icon={revealed ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
					onClick={() => setRevealed((r) => !r)}
					size="sm"
					variant="ghost"
				>
					{revealed ? "Hide" : "Show"}
				</Button>
			)}
			<Button icon={<Copy className="h-3.5 w-3.5" />} onClick={copy} size="sm" variant="ghost">
				Copy
			</Button>
		</div>
	)
}
