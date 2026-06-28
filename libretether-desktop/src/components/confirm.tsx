import { createContext, type ReactNode, useCallback, useContext, useRef, useState } from "react"
import { Button, Modal } from "./ui"

interface ConfirmOptions {
	title: string
	message?: ReactNode
	confirmLabel?: string
	cancelLabel?: string
	tone?: "primary" | "danger"
}

type ConfirmFn = (opts: ConfirmOptions) => Promise<boolean>

const ConfirmContext = createContext<ConfirmFn | null>(null)

export function ConfirmProvider({ children }: { children: ReactNode }) {
	const [opts, setOpts] = useState<ConfirmOptions | null>(null)
	const resolver = useRef<((v: boolean) => void) | null>(null)

	const confirm = useCallback<ConfirmFn>((options) => {
		setOpts(options)
		return new Promise<boolean>((resolve) => {
			resolver.current = resolve
		})
	}, [])

	const close = useCallback((value: boolean) => {
		resolver.current?.(value)
		resolver.current = null
		setOpts(null)
	}, [])

	return (
		<ConfirmContext.Provider value={confirm}>
			{children}
			<Modal
				footer={
					<>
						<Button onClick={() => close(false)} variant="ghost">
							{opts?.cancelLabel ?? "Cancel"}
						</Button>
						<Button onClick={() => close(true)} variant={opts?.tone === "danger" ? "danger" : "primary"}>
							{opts?.confirmLabel ?? "Confirm"}
						</Button>
					</>
				}
				onClose={() => close(false)}
				open={!!opts}
				size="sm"
				title={opts?.title}
			>
				<div className="text-sm leading-relaxed text-muted">{opts?.message}</div>
			</Modal>
		</ConfirmContext.Provider>
	)
}

export function useConfirm(): ConfirmFn {
	const ctx = useContext(ConfirmContext)
	if (!ctx) throw new Error("useConfirm must be used within ConfirmProvider")
	return ctx
}
