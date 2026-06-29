import { useCallback, useState } from "react"
import * as api from "./api"
import { useToast } from "./toast"

/** Wraps a one-shot async action with busy state + an error toast, removing the
 *  `setBusy(true) … catch → toast.error … finally setBusy(false)` boilerplate that
 *  every button handler would otherwise repeat. Returns `{ busy, run }`; use one
 *  instance per independently-tracked action.
 *
 *  `run` resolves to `true` when the action succeeded (handy for closing a modal
 *  only on success) and `false` when it threw. */
export function useAsyncAction() {
	const toast = useToast()
	const [busy, setBusy] = useState(false)

	const run = useCallback(
		async (errorTitle: string, fn: () => Promise<void>): Promise<boolean> => {
			setBusy(true)
			try {
				await fn()
				return true
			} catch (e) {
				toast.error(errorTitle, api.errString(e))
				return false
			} finally {
				setBusy(false)
			}
		},
		[toast]
	)

	return { busy, run }
}
