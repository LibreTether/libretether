import { useCallback, useEffect, useState } from "react"
import * as api from "./api"
import type { TransferItem, TransferProgress } from "./types"

/** Load the transfer queue and keep it live: the list refreshes on `transfers:changed`
 *  (enqueue / status / removal), and per-transfer byte progress arrives on the global
 *  `transfer:progress` event, merged into a map keyed by transfer id. */
export function useTransfers(): {
	transfers: TransferItem[]
	progress: Record<string, TransferProgress>
	reload: () => void
} {
	const [transfers, setTransfers] = useState<TransferItem[]>([])
	const [progress, setProgress] = useState<Record<string, TransferProgress>>({})

	const reload = useCallback(() => {
		api.listTransfers()
			.then(setTransfers)
			.catch(() => {})
	}, [])

	useEffect(() => {
		reload()
		const unlisteners = [
			api.onTransfersChanged(reload),
			api.onTransferProgress((p) => setProgress((m) => ({ ...m, [p.id]: p })))
		]
		return () => {
			for (const u of unlisteners) u.then((fn) => fn())
		}
	}, [reload])

	return { progress, reload, transfers }
}
