import { Loader2, X } from "lucide-react"
import { useEffect, useRef, useState } from "react"
import * as api from "../lib/api"
import type { ActiveInfo, ControllerSummary } from "../lib/types"
import { Button } from "./ui"

/** Shown while a relay controller establishes its connection. Streams the
 *  relay's connection log and only hands control to the main screen once the
 *  relay accepts; Cancel tears the attempt down and returns to the launch screen. */
export function RelayConnecting({
	controller,
	onConnected,
	onCancel
}: {
	controller: ControllerSummary
	onConnected: (a: ActiveInfo) => void
	onCancel: () => void
}) {
	const [logs, setLogs] = useState<string[]>([])
	const logRef = useRef<HTMLDivElement>(null)

	useEffect(() => {
		let alive = true
		const unlisteners = [
			api.onControllerLog((line) => alive && setLogs((prev) => [...prev, line])),
			api.onControllerConnected(async () => {
				if (!alive) return
				const info = await api.activeController()
				if (alive && info) onConnected(info)
			})
		]
		// Start serving — this kicks off the relay dial; progress arrives as events.
		api.selectController(controller.id).catch(
			(e) => alive && setLogs((prev) => [...prev, `error: ${api.errString(e)}`])
		)
		return () => {
			alive = false
			for (const u of unlisteners) u.then((fn) => fn())
		}
	}, [controller.id, onConnected])

	useEffect(() => {
		if (logs.length) logRef.current?.scrollTo({ top: logRef.current.scrollHeight })
	}, [logs])

	const cancel = async () => {
		try {
			await api.exitController()
		} catch {
			/* tearing down regardless */
		}
		onCancel()
	}

	return (
		<div className="flex h-screen flex-col overflow-hidden">
			<div className="drag h-9 shrink-0" />
			<div className="flex min-h-0 flex-1 items-center justify-center p-6">
				<div className="flex w-full max-w-lg flex-col gap-4">
					<div className="flex flex-col items-center gap-2 text-center">
						<Loader2 className="h-7 w-7 animate-spin text-primary dark:text-primary-strong" />
						<h1 className="text-lg font-bold text-text">Connecting to “{controller.name}”…</h1>
						<p className="text-sm text-muted">
							Establishing the relay connection — you'll go to the controller once the relay accepts.
						</p>
					</div>
					<div
						className="card max-h-64 min-h-[6rem] overflow-y-auto p-3 font-mono text-xs leading-relaxed text-muted"
						ref={logRef}
					>
						{logs.length === 0 ? (
							<span className="text-subtle">Starting…</span>
						) : (
							logs.map((line, i) => (
								<div className="whitespace-pre-wrap break-all" key={i}>
									{line}
								</div>
							))
						)}
					</div>
					<div className="flex justify-center">
						<Button icon={<X className="h-4 w-4" />} onClick={cancel} variant="outline">
							Cancel
						</Button>
					</div>
				</div>
			</div>
		</div>
	)
}
