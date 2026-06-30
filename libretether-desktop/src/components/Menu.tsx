import { EllipsisVertical } from "lucide-react"
import { type ReactNode, useEffect, useRef, useState } from "react"
import { createPortal } from "react-dom"
import { cn } from "../lib/cn"
import { Button } from "./ui"

export interface MenuItem {
	key: string
	label: string
	icon?: ReactNode
	onSelect: () => void
	danger?: boolean
	disabled?: boolean
}

const MENU_WIDTH = 196

/** A ⋯ overflow menu for a row's secondary actions. Rendered through a portal with
 *  fixed positioning so it isn't clipped by the scrolling list it lives in; it
 *  right-aligns to the trigger and flips above when there's no room below. */
export function Menu({ items, title = "More actions" }: { items: MenuItem[]; title?: string }) {
	const [open, setOpen] = useState(false)
	const [pos, setPos] = useState<{ top: number; left: number } | null>(null)
	const triggerRef = useRef<HTMLButtonElement>(null)
	const menuRef = useRef<HTMLDivElement>(null)

	const place = () => {
		const t = triggerRef.current
		if (!t) return
		const r = t.getBoundingClientRect()
		const estHeight = items.length * 38 + 12
		const left = Math.max(8, Math.min(r.right - MENU_WIDTH, window.innerWidth - MENU_WIDTH - 8))
		const below = r.bottom + 6
		const top = below + estHeight > window.innerHeight - 8 ? Math.max(8, r.top - 6 - estHeight) : below
		setPos({ left, top })
	}

	const toggle = () => {
		if (open) {
			setOpen(false)
		} else {
			place()
			setOpen(true)
		}
	}

	useEffect(() => {
		if (!open) return
		const onDown = (e: MouseEvent) => {
			const target = e.target as Node
			if (!menuRef.current?.contains(target) && !triggerRef.current?.contains(target)) setOpen(false)
		}
		const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false)
		// The menu is anchored to a row in a scrolling list; rather than re-track the
		// trigger on every scroll, dismiss it (matches how most desktop menus behave).
		const dismiss = () => setOpen(false)
		window.addEventListener("mousedown", onDown)
		window.addEventListener("keydown", onKey)
		window.addEventListener("resize", dismiss)
		window.addEventListener("scroll", dismiss, true)
		return () => {
			window.removeEventListener("mousedown", onDown)
			window.removeEventListener("keydown", onKey)
			window.removeEventListener("resize", dismiss)
			window.removeEventListener("scroll", dismiss, true)
		}
	}, [open])

	return (
		<>
			<Button
				className={cn(open && "bg-surface-3 text-text")}
				icon={<EllipsisVertical className="h-4 w-4" />}
				onClick={toggle}
				ref={triggerRef}
				size="icon-sm"
				title={title}
				variant="ghost"
			/>
			{open &&
				pos &&
				createPortal(
					<div
						className="card fixed z-[90] overflow-hidden p-1 shadow-xl shadow-black/40"
						ref={menuRef}
						style={{ animation: "var(--animate-fade-in)", left: pos.left, top: pos.top, width: MENU_WIDTH }}
					>
						{items.map((it) => (
							<button
								className={cn(
									"flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-left text-sm transition disabled:opacity-40",
									it.danger
										? "text-danger hover:bg-danger-soft"
										: "text-muted hover:bg-surface-3 hover:text-text"
								)}
								disabled={it.disabled}
								key={it.key}
								onClick={() => {
									setOpen(false)
									it.onSelect()
								}}
								type="button"
							>
								{it.icon && (
									<span className="flex h-4 w-4 shrink-0 items-center justify-center">{it.icon}</span>
								)}
								<span className="min-w-0 flex-1 truncate">{it.label}</span>
							</button>
						))}
					</div>,
					document.body
				)}
		</>
	)
}
