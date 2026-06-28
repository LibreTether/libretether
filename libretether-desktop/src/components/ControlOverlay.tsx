import { Keyboard, Loader2, MousePointer2, Power } from "lucide-react"
import { useCallback, useEffect, useRef, useState } from "react"
import * as api from "../lib/api"
import type { ClientDto, InputEvent, MouseButton, SessionMeta } from "../lib/types"

const BUTTONS: Record<number, MouseButton> = { 0: "left", 1: "middle", 2: "right" }

export function ControlOverlay({ client, onClose }: { client: ClientDto; onClose: () => void }) {
	const imgRef = useRef<HTMLImageElement>(null)
	const surfaceRef = useRef<HTMLDivElement>(null)
	const [meta, setMeta] = useState<SessionMeta | null>(null)
	const [error, setError] = useState<string | null>(null)
	const [frames, setFrames] = useState(0)

	// Coalesce pointer moves to one send per animation frame.
	const pendingMove = useRef<{ x: number; y: number } | null>(null)
	const rafScheduled = useRef(false)

	const send = useCallback(
		(event: InputEvent) => {
			api.sendInput(client.id, event).catch(() => {
				/* session closing — ignore */
			})
		},
		[client.id]
	)

	// Subscribe to session events, then start the session.
	useEffect(() => {
		let alive = true
		const unlisteners: Promise<() => void>[] = [
			api.onSessionMeta(client.id, (m) => alive && setMeta(m)),
			api.onSessionFrame(client.id, (f) => {
				if (!alive || !imgRef.current) return
				imgRef.current.src = `data:image/jpeg;base64,${f.data_base64}`
				setFrames((n) => n + 1)
			}),
			api.onSessionError(client.id, (msg) => alive && setError(msg)),
			api.onSessionClosed(client.id, () => alive && setError((prev) => prev ?? "The session ended."))
		]

		// Defer the start by a tick so React StrictMode's throwaway mount (which
		// unmounts immediately) never actually opens a session — avoids a second
		// portal consent dialog on the client.
		const startTimer = setTimeout(() => {
			if (alive)
				api.startControl(client.id, { maxFps: 20, quality: 70 }).catch(
					(e) => alive && setError(api.errString(e))
				)
		}, 0)
		surfaceRef.current?.focus()

		return () => {
			alive = false
			clearTimeout(startTimer)
			api.stopControl(client.id).catch(() => {})
			for (const u of unlisteners) u.then((fn) => fn())
		}
	}, [client.id])

	const norm = (e: React.PointerEvent | React.MouseEvent): { x: number; y: number } | null => {
		const el = imgRef.current
		if (!el) return null
		const r = el.getBoundingClientRect()
		if (r.width === 0 || r.height === 0) return null
		return { x: (e.clientX - r.left) / r.width, y: (e.clientY - r.top) / r.height }
	}

	const onMove = (e: React.PointerEvent) => {
		const p = norm(e)
		if (!p) return
		pendingMove.current = p
		if (!rafScheduled.current) {
			rafScheduled.current = true
			requestAnimationFrame(() => {
				rafScheduled.current = false
				if (pendingMove.current) send({ t: "mouse_move", x: pendingMove.current.x, y: pendingMove.current.y })
			})
		}
	}

	const onButton = (e: React.PointerEvent, pressed: boolean) => {
		const button = BUTTONS[e.button]
		if (!button) return
		const p = norm(e)
		if (p) send({ t: "mouse_move", x: p.x, y: p.y })
		send({ button, pressed, t: "mouse_button" })
	}

	const onWheel = (e: React.WheelEvent) => {
		const dy = Math.round(e.deltaY / 100) || Math.sign(e.deltaY)
		const dx = Math.round(e.deltaX / 100) || Math.sign(e.deltaX)
		if (dx || dy) send({ dx, dy, t: "mouse_scroll" })
	}

	const onKey = (e: React.KeyboardEvent, pressed: boolean) => {
		// Let the overlay's own Escape-to-close work without forwarding it.
		if (e.key === "Escape") {
			if (pressed) onClose()
			return
		}
		e.preventDefault()
		send({ code: e.code, pressed, t: "key" })
	}

	return (
		<div className="fixed inset-0 z-[60] flex flex-col bg-black/95" style={{ animation: "var(--animate-fade-in)" }}>
			<div className="no-drag flex items-center gap-3 border-b border-white/10 px-4 py-2.5 text-white">
				<MousePointer2 className="h-4 w-4 text-primary-strong" />
				<span className="font-semibold">{client.name}</span>
				<span className="text-xs text-white/50">
					{meta ? `${meta.width}×${meta.height}` : "connecting…"}
					{frames > 0 && ` · ${frames} frames`}
				</span>
				<span className="ml-auto flex items-center gap-1.5 text-xs text-white/50">
					<Keyboard className="h-3.5 w-3.5" /> click the screen, then type to control
				</span>
				<button
					className="ml-2 flex items-center gap-1.5 rounded-lg bg-danger/90 px-3 py-1.5 text-xs font-semibold text-white transition hover:bg-danger"
					onClick={onClose}
				>
					<Power className="h-3.5 w-3.5" /> Disconnect
				</button>
			</div>

			<div className="relative flex min-h-0 flex-1 items-center justify-center overflow-hidden">
				{!meta && !error && (
					<div className="absolute flex items-center gap-2 text-sm text-white/70">
						<Loader2 className="h-4 w-4 animate-spin" /> Starting session…
					</div>
				)}
				{error && (
					<div className="absolute z-10 flex flex-col items-center gap-3 rounded-2xl bg-surface p-6 text-center">
						<p className="text-sm font-medium text-danger">{error}</p>
						<button
							className="rounded-lg bg-primary px-4 py-2 text-sm font-semibold text-white"
							onClick={onClose}
						>
							Close
						</button>
					</div>
				)}
				<div
					className="flex h-full w-full items-center justify-center outline-none"
					onKeyDown={(e) => onKey(e, true)}
					onKeyUp={(e) => onKey(e, false)}
					ref={surfaceRef}
					tabIndex={0}
				>
					<img
						alt="remote screen"
						className="max-h-full max-w-full select-none"
						draggable={false}
						onContextMenu={(e) => e.preventDefault()}
						onPointerDown={(e) => {
							surfaceRef.current?.focus()
							onButton(e, true)
						}}
						onPointerMove={onMove}
						onPointerUp={(e) => onButton(e, false)}
						onWheel={onWheel}
						ref={imgRef}
						style={{ imageRendering: "auto" }}
					/>
				</div>
			</div>
		</div>
	)
}
