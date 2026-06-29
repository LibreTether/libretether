import { useCallback, useEffect, useMemo, useRef, useState } from "react"
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
	// A consumer can unmount (modal closed, navigation away) while an `invoke` is
	// still in flight; settling `setBusy` then warns and writes to a dead component.
	// Guard the post-await state writes on whether we're still mounted.
	const mounted = useRef(true)
	useEffect(() => {
		mounted.current = true
		return () => {
			mounted.current = false
		}
	}, [])

	const run = useCallback(
		async (errorTitle: string, fn: () => Promise<void>): Promise<boolean> => {
			setBusy(true)
			try {
				await fn()
				return true
			} catch (e) {
				if (mounted.current) toast.error(errorTitle, api.errString(e))
				return false
			} finally {
				if (mounted.current) setBusy(false)
			}
		},
		[toast]
	)

	// Return a referentially-stable object (changing only when `busy`/`run` do) so
	// consumers can safely depend on the whole action in effect deps without churn.
	return useMemo(() => ({ busy, run }), [busy, run])
}
