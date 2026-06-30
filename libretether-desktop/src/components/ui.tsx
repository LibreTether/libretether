import { Loader2, X } from "lucide-react"
import { type ButtonHTMLAttributes, forwardRef, type InputHTMLAttributes, type ReactNode, useEffect } from "react"
import { cn } from "../lib/cn"

// ----------------------------------------------------------------- Spinner
export function Spinner({ className }: { className?: string }) {
	return <Loader2 className={cn("h-4 w-4 animate-spin", className)} />
}

// ----------------------------------------------------------------- Button
type Variant = "primary" | "soft" | "ghost" | "danger" | "success" | "outline"
type Size = "sm" | "md" | "lg" | "icon" | "icon-sm"

const VARIANTS: Record<Variant, string> = {
	danger: "bg-danger text-white hover:brightness-110",
	ghost: "text-muted hover:bg-surface-3 hover:text-text",
	outline: "border border-border-strong bg-surface text-text hover:bg-surface-2",
	primary: "bg-primary text-primary-fg shadow-sm shadow-black/20 hover:bg-primary-strong",
	soft: "bg-primary-soft text-primary hover:brightness-105 dark:text-primary-strong",
	success: "bg-success text-white hover:brightness-110"
}

const SIZES: Record<Size, string> = {
	icon: "h-9.5 w-9.5 p-0",
	"icon-sm": "h-8 w-8 p-0 rounded-lg",
	lg: "h-11 px-5 text-sm",
	md: "h-9.5 px-3.5 text-sm",
	sm: "h-8 px-3 text-xs rounded-lg"
}

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
	variant?: Variant
	size?: Size
	loading?: boolean
	icon?: ReactNode
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
	{ variant = "soft", size = "md", loading, icon, className, children, disabled, ...rest },
	ref
) {
	// Default to a non-submit button (the app has no <form>s); a caller can still
	// override `type` via `...rest`.
	return (
		<button
			className={cn("btn no-drag", VARIANTS[variant], SIZES[size], className)}
			disabled={disabled || loading}
			ref={ref}
			type="button"
			{...rest}
		>
			{loading ? <Spinner /> : icon}
			{children}
		</button>
	)
})

// ----------------------------------------------------------------- Kbd
/** A keycap for shortcut hints. Children are short labels like "K", "⌘", "Esc". */
export function Kbd({ children, className }: { children: ReactNode; className?: string }) {
	return <kbd className={cn("kbd", className)}>{children}</kbd>
}

// ----------------------------------------------------------------- StatusDot
/** The fleet's pulse. Online dots breathe — the heartbeat of a held connection;
 *  pending is the actionable "awaiting enrollment" amber; offline is a quiet ring. */
export function StatusDot({ state, className }: { state: "online" | "offline" | "pending"; className?: string }) {
	if (state === "online") {
		return <span className={cn("signal-live h-2.5 w-2.5 rounded-full bg-success", className)} />
	}
	if (state === "pending") {
		return <span className={cn("h-2.5 w-2.5 rounded-full bg-primary", className)} />
	}
	return <span className={cn("h-2.5 w-2.5 rounded-full border-2 border-subtle/60", className)} />
}

// ----------------------------------------------------------------- Badge
export function Badge({
	children,
	className,
	tone = "neutral"
}: {
	children: ReactNode
	className?: string
	tone?: "neutral" | "success" | "warning" | "danger" | "primary" | "accent"
}) {
	const tones = {
		accent: "bg-accent/12 text-accent",
		danger: "bg-danger-soft text-danger",
		neutral: "bg-surface-3 text-muted",
		primary: "bg-primary-soft text-primary dark:text-primary-strong",
		success: "bg-success-soft text-success",
		warning: "bg-warning-soft text-warning"
	}
	return (
		<span
			className={cn(
				"inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[0.7rem] font-semibold",
				tones[tone],
				className
			)}
		>
			{children}
		</span>
	)
}

// ----------------------------------------------------------------- Field
export function Field({
	label,
	hint,
	children,
	className
}: {
	label?: string
	hint?: string
	children: ReactNode
	className?: string
}) {
	return (
		<label className={cn("flex flex-col gap-1.5", className)}>
			{label && <span className="text-xs font-semibold text-muted">{label}</span>}
			{children}
			{hint && <span className="text-[0.72rem] leading-relaxed text-subtle">{hint}</span>}
		</label>
	)
}

const inputBase =
	"no-drag w-full rounded-lg border border-border bg-surface-2 px-3 py-2 text-sm text-text outline-none transition placeholder:text-subtle focus:border-primary focus:ring-2 focus:ring-ring/25 disabled:opacity-50"

export const Input = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement>>(function Input(
	{ className, ...rest },
	ref
) {
	return <input className={cn(inputBase, className)} ref={ref} {...rest} />
})

// ----------------------------------------------------------------- Modal
export function Modal({
	open,
	onClose,
	title,
	children,
	footer,
	className,
	size = "md"
}: {
	open: boolean
	onClose: () => void
	title?: ReactNode
	children: ReactNode
	footer?: ReactNode
	className?: string
	size?: "sm" | "md" | "lg" | "xl"
}) {
	useEffect(() => {
		if (!open) return
		const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose()
		window.addEventListener("keydown", onKey)
		return () => window.removeEventListener("keydown", onKey)
	}, [open, onClose])

	if (!open) return null

	const widths = {
		lg: "max-w-2xl",
		md: "max-w-lg",
		sm: "max-w-md",
		xl: "max-w-4xl"
	}

	return (
		<div
			className="fixed inset-0 z-50 flex items-center justify-center p-5"
			style={{ animation: "var(--animate-fade-in)" }}
		>
			<div className="absolute inset-0 bg-black/55 backdrop-blur-sm" onClick={onClose} />
			<div
				aria-modal="true"
				className={cn(
					"card relative z-10 flex max-h-[88vh] w-full flex-col overflow-hidden shadow-2xl shadow-black/40",
					widths[size],
					className
				)}
				role="dialog"
				style={{ animation: "var(--animate-slide-up)" }}
			>
				{title && (
					<div className="flex items-center justify-between border-b border-border px-5 py-3.5">
						<h2 className="font-display text-base font-semibold text-text">{title}</h2>
						<button
							aria-label="Close"
							className="no-drag rounded-lg p-1.5 text-subtle transition hover:bg-surface-3 hover:text-text"
							onClick={onClose}
							type="button"
						>
							<X className="h-4.5 w-4.5" />
						</button>
					</div>
				)}
				<div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">{children}</div>
				{footer && (
					<div className="flex items-center justify-end gap-2.5 border-t border-border bg-surface-2/60 px-5 py-3.5">
						{footer}
					</div>
				)}
			</div>
		</div>
	)
}

// ----------------------------------------------------------------- Drawer
/** A right-side slide-over. Lighter than a modal: it doesn't trap the whole
 *  screen behind a heavy scrim, and Esc / backdrop-click dismiss it. Used for
 *  machine details and the add-machine flow so the app leans on panels, not
 *  stacked dialogs. */
export function Drawer({
	open,
	onClose,
	title,
	subtitle,
	children,
	footer,
	icon,
	size = "md"
}: {
	open: boolean
	onClose: () => void
	title?: ReactNode
	subtitle?: ReactNode
	children: ReactNode
	footer?: ReactNode
	icon?: ReactNode
	size?: "md" | "lg"
}) {
	useEffect(() => {
		if (!open) return
		const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose()
		window.addEventListener("keydown", onKey)
		return () => window.removeEventListener("keydown", onKey)
	}, [open, onClose])

	if (!open) return null

	const widths = { lg: "max-w-2xl", md: "max-w-md" }

	return (
		<div className="fixed inset-0 z-50 flex justify-end" style={{ animation: "var(--animate-fade-in)" }}>
			<div className="absolute inset-0 bg-black/45 backdrop-blur-[2px]" onClick={onClose} />
			<aside
				aria-modal="true"
				className={cn(
					"relative z-10 flex h-full w-full flex-col border-l border-border bg-surface shadow-2xl shadow-black/40",
					widths[size]
				)}
				role="dialog"
				style={{ animation: "var(--animate-drawer-in)" }}
			>
				<div className="flex items-start gap-3 border-b border-border px-5 py-4">
					{icon && (
						<div className="grid h-10 w-10 shrink-0 place-items-center rounded-xl bg-surface-2 text-primary dark:text-primary-strong">
							{icon}
						</div>
					)}
					<div className="min-w-0 flex-1">
						<h2 className="truncate font-display text-base font-semibold text-text">{title}</h2>
						{subtitle && <div className="mt-0.5 truncate text-xs text-muted">{subtitle}</div>}
					</div>
					<button
						aria-label="Close"
						className="no-drag rounded-lg p-1.5 text-subtle transition hover:bg-surface-3 hover:text-text"
						onClick={onClose}
						type="button"
					>
						<X className="h-4.5 w-4.5" />
					</button>
				</div>
				<div className="min-h-0 flex-1 overflow-y-auto px-5 py-5">{children}</div>
				{footer && (
					<div className="flex items-center justify-end gap-2.5 border-t border-border bg-surface-2/50 px-5 py-3.5">
						{footer}
					</div>
				)}
			</aside>
		</div>
	)
}

// ----------------------------------------------------------------- EmptyState
export function EmptyState({
	icon,
	title,
	description,
	action
}: {
	icon: ReactNode
	title: string
	description?: ReactNode
	action?: ReactNode
}) {
	return (
		<div className="flex flex-col items-center justify-center gap-3 px-6 py-16 text-center">
			<div className="grid h-16 w-16 place-items-center rounded-2xl border border-border bg-surface-2 text-subtle">
				{icon}
			</div>
			<div>
				<h3 className="font-display text-base font-semibold text-text">{title}</h3>
				{description && <p className="mx-auto mt-1 max-w-sm text-sm text-muted">{description}</p>}
			</div>
			{action}
		</div>
	)
}
