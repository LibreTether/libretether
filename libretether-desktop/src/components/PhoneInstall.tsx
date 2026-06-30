import { Check, Globe, Hash, ShieldCheck, X } from "lucide-react"
import { useEffect, useState } from "react"
import * as api from "../lib/api"
import type { PairingStarted } from "../lib/types"
import { CopyRow } from "./CopyRow"
import { Spinner } from "./ui"

/** The phone-install view: the operator reads the site + code aloud, and this panel
 *  shows live progress as the machine pairs. The pairing runs in the background on
 *  the controller (the PAKE over the relay); we listen for its `pairing:completed`
 *  event, matched by `code` so a stale pairing can't flip this one's status. */
export function PhoneInstall({ started }: { started: PairingStarted }) {
	const [state, setState] = useState<"waiting" | "done" | "failed">("waiting")
	const [phrase, setPhrase] = useState<string | null>(null)
	const [error, setError] = useState<string | null>(null)

	useEffect(() => {
		const unlisten = api.onPairingCompleted((e) => {
			if (e.code !== started.code) return // not our pairing
			if (e.ok) {
				setPhrase(e.phrase ?? null)
				setState("done")
			} else {
				setError(e.error ?? "Pairing didn't complete.")
				setState("failed")
			}
		})
		return () => {
			unlisten.then((fn) => fn())
		}
	}, [started.code])

	// The host without the scheme reads better aloud ("go to relay.example.com").
	const host = started.portal_url.replace(/^https?:\/\//, "")

	return (
		<div className="flex flex-col gap-4">
			<div className="flex items-start gap-2.5 rounded-xl border border-primary-soft bg-primary-soft/50 px-3.5 py-3 text-sm text-muted">
				<ShieldCheck className="mt-0.5 h-4 w-4 shrink-0 text-primary dark:text-primary-strong" />
				<p className="leading-relaxed">
					Read these out to the person at the computer. They open the site, type the code, and run the
					installer it gives them — no command to paste. Keep this window open until it pairs.
				</p>
			</div>

			<div className="flex flex-col gap-2.5">
				<div className="flex items-center gap-2 text-xs font-semibold text-muted">
					<Globe className="h-3.5 w-3.5 text-primary dark:text-primary-strong" /> 1 · Go to this site
				</div>
				<CopyRow value={host} />
			</div>

			<div className="flex flex-col gap-2.5">
				<div className="flex items-center gap-2 text-xs font-semibold text-muted">
					<Hash className="h-3.5 w-3.5 text-primary dark:text-primary-strong" /> 2 · Enter this code
				</div>
				<div className="rounded-xl border border-border bg-surface-2 px-4 py-4 text-center">
					<span className="select-text font-mono text-2xl font-bold tracking-[0.18em] text-text">
						{started.code}
					</span>
				</div>
			</div>

			{/* Live status. */}
			<div className="rounded-xl border border-border bg-surface px-3.5 py-3">
				{state === "waiting" && (
					<div className="flex items-center gap-2.5 text-sm text-muted">
						<Spinner className="h-4 w-4" />
						Waiting for the machine to pair…
					</div>
				)}
				{state === "done" && (
					<div className="flex flex-col gap-1.5">
						<div className="flex items-center gap-2 text-sm font-semibold text-success">
							<Check className="h-4 w-4" /> Paired — this machine is connecting now.
						</div>
						{phrase && (
							<p className="text-[0.8rem] leading-relaxed text-muted">
								Verify phrase <code className="font-mono text-text">{phrase}</code> — it should match
								what the person sees on their screen.
							</p>
						)}
					</div>
				)}
				{state === "failed" && (
					<div className="flex items-start gap-2 text-sm text-danger">
						<X className="mt-0.5 h-4 w-4 shrink-0" />
						<span>{error} The code may have expired — close and start over for a fresh one.</span>
					</div>
				)}
			</div>
		</div>
	)
}
