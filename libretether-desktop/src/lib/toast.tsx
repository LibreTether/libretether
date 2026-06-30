import { CheckCircle2, Info, TriangleAlert, X, XCircle } from "lucide-react"
import { createContext, type ReactNode, useCallback, useContext, useMemo, useRef, useState } from "react"
import { cn } from "./cn"

export type ToastKind = "success" | "error" | "info" | "warning"

export interface Toast {
	id: number
	kind: ToastKind
	title: string
	message?: string
}

interface ToastContextValue {
	push: (t: Omit<Toast, "id">) => number
	dismiss: (id: number) => void
	success: (title: string, message?: string) => number
	error: (title: string, message?: string) => number
	info: (title: string, message?: string) => number
	warning: (title: string, message?: string) => number
}

const ToastContext = createContext<ToastContextValue | null>(null)

const ICONS = {
	error: XCircle,
	info: Info,
	success: CheckCircle2,
	warning: TriangleAlert
} as const

const ACCENT = {
	error: "text-danger",
	info: "text-accent",
	success: "text-success",
	warning: "text-warning"
} as const

export function ToastProvider({ children }: { children: ReactNode }) {
	const [toasts, setToasts] = useState<Toast[]>([])
	const counter = useRef(0)

	const dismiss = useCallback((id: number) => {
		setToasts((prev) => prev.filter((t) => t.id !== id))
	}, [])

	const push = useCallback(
		(t: Omit<Toast, "id">) => {
			const id = ++counter.current
			setToasts((prev) => [...prev, { ...t, id }])
			const ttl = t.kind === "error" ? 8000 : 4500
			window.setTimeout(() => dismiss(id), ttl)
			return id
		},
		[dismiss]
	)

	const value = useMemo<ToastContextValue>(
		() => ({
			dismiss,
			error: (title, message) => push({ kind: "error", message, title }),
			info: (title, message) => push({ kind: "info", message, title }),
			push,
			success: (title, message) => push({ kind: "success", message, title }),
			warning: (title, message) => push({ kind: "warning", message, title })
		}),
		[push, dismiss]
	)

	return (
		<ToastContext.Provider value={value}>
			{children}
			<div className="pointer-events-none fixed bottom-5 right-5 z-[100] flex w-[min(26rem,calc(100vw-2.5rem))] flex-col gap-2.5">
				{toasts.map((t) => {
					const Icon = ICONS[t.kind]
					return (
						<div
							className="card glass pointer-events-auto flex items-start gap-3 p-3.5 shadow-xl shadow-black/10"
							key={t.id}
							style={{ animation: "var(--animate-slide-up)" }}
						>
							<Icon className={cn("mt-0.5 h-5 w-5 shrink-0", ACCENT[t.kind])} />
							<div className="min-w-0 flex-1">
								<p className="text-sm font-semibold text-text">{t.title}</p>
								{t.message && (
									<p className="mt-0.5 line-clamp-4 break-words text-xs text-muted">{t.message}</p>
								)}
							</div>
							<button
								className="rounded-md p-1 text-subtle transition hover:bg-surface-3 hover:text-text"
								onClick={() => dismiss(t.id)}
							>
								<X className="h-4 w-4" />
							</button>
						</div>
					)
				})}
			</div>
		</ToastContext.Provider>
	)
}

export function useToast(): ToastContextValue {
	const ctx = useContext(ToastContext)
	if (!ctx) throw new Error("useToast must be used within ToastProvider")
	return ctx
}
