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
	// Count frames in a ref and surface a frames-per-second readout ~1 Hz, so a
	// 20 fps stream doesn't re-render the whole overlay on every frame (the <img>
	// is updated imperatively). `prevFramesRef` holds the last tick's total so the
	// interval can report the delta (an fps), not an ever-growing cumulative count.
	const framesRef = useRef(0)
	const prevFramesRef = useRef(0)
	const [fps, setFps] = useState(0)

	// Coalesce pointer moves to one send per animation frame.
	const pendingMove = useRef<{ x: number; y: number } | null>(null)
	const rafScheduled = useRef(false)

	// What the controller currently holds down. Tracked so we can release a key or
	// mouse button that's still pressed when control ends (overlay close, the host
	// window losing focus, a pointer-up that lands outside the image) — otherwise a
	// held Ctrl/Shift or mouse button stays stuck *down* on the remote machine.
	const pressedKeys = useRef<Set<string>>(new Set())
	const pressedButtons = useRef<Set<MouseButton>>(new Set())
	// Timestamp of the last Escape keydown, so a quick double-tap exits the overlay
	// while a single Escape is forwarded to the remote (a remote-control surface must
	// be able to send Escape — close vim insert mode, dismiss a dialog, …).
	const escapeAt = useRef(0)

	const send = useCallback(
		(event: InputEvent) => {
			api.sendInput(client.id, event).catch(() => {
				/* session closing — ignore */
			})
		},
		[client.id]
	)

	// Release everything currently held, then forget it. Safe to call repeatedly.
	const releaseAll = useCallback(() => {
		for (const code of pressedKeys.current) send({ code, pressed: false, t: "key" })
		pressedKeys.current.clear()
		for (const button of pressedButtons.current) send({ button, pressed: false, t: "mouse_button" })
		pressedButtons.current.clear()
	}, [send])

	// Subscribe to session events, then start the session.
	useEffect(() => {
		let alive = true
		const unlisteners: Promise<() => void>[] = [
			api.onSessionMeta(client.id, (m) => alive && setMeta(m)),
			api.onSessionFrame(client.id, (f) => {
				if (!alive || !imgRef.current) return
				imgRef.current.src = `data:image/${f.encoding};base64,${f.data_base64}`
				framesRef.current += 1
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
			// Flush any held keys/buttons *before* tearing the session down so the
			// releases still reach the agent (the agent also releases on stop as a
			// backstop, but this covers both backends and is immediate).
			releaseAll()
			api.stopControl(client.id).catch(() => {})
			for (const u of unlisteners) u.then((fn) => fn())
		}
	}, [client.id, releaseAll])

	// Report fps ~1 Hz as the delta since the last tick (see above).
	useEffect(() => {
		const t = window.setInterval(() => {
			setFps(framesRef.current - prevFramesRef.current)
			prevFramesRef.current = framesRef.current
		}, 1000)
		return () => window.clearInterval(t)
	}, [])

	// Keyboard is handled at the window level (not on the focused surface) so that
	// control doesn't silently die the moment focus moves to the header, a button,
	// or a toast. Escape closes the overlay; everything else is forwarded.
	useEffect(() => {
		const onKeyDown = (e: KeyboardEvent) => {
			e.preventDefault()
			// A double-tap of Escape (two presses within 500ms) exits the overlay; a
			// single Escape falls through and is forwarded to the remote like any key.
			if (e.key === "Escape" && !e.repeat) {
				if (e.timeStamp - escapeAt.current < 500) {
					onClose()
					return
				}
				escapeAt.current = e.timeStamp
			}
			pressedKeys.current.add(e.code)
			send({ code: e.code, pressed: true, t: "key" })
		}
		const onKeyUp = (e: KeyboardEvent) => {
			e.preventDefault()
			pressedKeys.current.delete(e.code)
			send({ code: e.code, pressed: false, t: "key" })
		}
		// The host window losing focus (alt-tab) means we'll never see the keyup, so
		// release now rather than strand a modifier on the remote.
		const onBlur = () => releaseAll()
		window.addEventListener("keydown", onKeyDown)
		window.addEventListener("keyup", onKeyUp)
		window.addEventListener("blur", onBlur)
		return () => {
			window.removeEventListener("keydown", onKeyDown)
			window.removeEventListener("keyup", onKeyUp)
			window.removeEventListener("blur", onBlur)
		}
	}, [send, releaseAll, onClose])

	// Forward wheel scroll to the agent and stop it from scrolling/zooming the
	// host webview. React's synthetic `onWheel` is passive (preventDefault is a
	// no-op there), so attach a non-passive native listener.
	useEffect(() => {
		const el = surfaceRef.current
		if (!el) return
		const onWheelNative = (e: WheelEvent) => {
			e.preventDefault()
			const dy = Math.round(e.deltaY / 100) || Math.sign(e.deltaY)
			const dx = Math.round(e.deltaX / 100) || Math.sign(e.deltaX)
			if (dx || dy) send({ dx, dy, t: "mouse_scroll" })
		}
		el.addEventListener("wheel", onWheelNative, { passive: false })
		return () => el.removeEventListener("wheel", onWheelNative)
	}, [send])

	const norm = (e: React.PointerEvent | React.MouseEvent): { x: number; y: number } | null => {
		const el = imgRef.current
		if (!el) return null
		const r = el.getBoundingClientRect()
		if (r.width === 0 || r.height === 0) return null
		// Clamp to [0,1]: a fast drag at the edge can land a pixel outside the image.
		const clamp = (v: number) => Math.min(1, Math.max(0, v))
		return { x: clamp((e.clientX - r.left) / r.width), y: clamp((e.clientY - r.top) / r.height) }
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

	const onPointerDown = (e: React.PointerEvent) => {
		const button = BUTTONS[e.button]
		if (!button) return
		// Capture the pointer so the matching pointerup still fires even if the user
		// drags off the image before releasing — otherwise the remote button sticks.
		e.currentTarget.setPointerCapture(e.pointerId)
		const p = norm(e)
		if (p) send({ t: "mouse_move", x: p.x, y: p.y })
		pressedButtons.current.add(button)
		send({ button, pressed: true, t: "mouse_button" })
	}

	const onPointerUp = (e: React.PointerEvent) => {
		const button = BUTTONS[e.button]
		if (!button) return
		const p = norm(e)
		if (p) send({ t: "mouse_move", x: p.x, y: p.y })
		pressedButtons.current.delete(button)
		send({ button, pressed: false, t: "mouse_button" })
	}

	// The gesture was cancelled (e.g. the OS took over) — release whatever's down.
	const onPointerCancel = () => {
		for (const button of pressedButtons.current) send({ button, pressed: false, t: "mouse_button" })
		pressedButtons.current.clear()
	}

	// Paste / IME-committed text: physical `code` events can't represent these, so
	// forward the committed string through the protocol's text channel.
	const onPaste = (e: React.ClipboardEvent) => {
		const text = e.clipboardData.getData("text")
		if (text) {
			e.preventDefault()
			send({ t: "text", text })
		}
	}

	return (
		<div className="fixed inset-0 z-[60] flex flex-col bg-black/95" style={{ animation: "var(--animate-fade-in)" }}>
			<div className="no-drag flex items-center gap-3 border-b border-white/10 px-4 py-2.5 text-white">
				<MousePointer2 className="h-4 w-4 text-primary-strong" />
				<span className="font-semibold">{client.name}</span>
				<span className="text-xs text-white/50">
					{meta ? `${meta.width}×${meta.height}` : "connecting…"}
					{fps > 0 && ` · ${fps} fps`}
				</span>
				<span className="ml-auto flex items-center gap-1.5 text-xs text-white/50">
					<Keyboard className="h-3.5 w-3.5" /> type to control · Esc×2 to exit
				</span>
				<button
					className="ml-2 flex items-center gap-1.5 rounded-lg bg-danger/90 px-3 py-1.5 text-xs font-semibold text-white transition hover:bg-danger"
					onClick={onClose}
					type="button"
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
							type="button"
						>
							Close
						</button>
					</div>
				)}
				<div
					className="flex h-full w-full items-center justify-center outline-none"
					onPaste={onPaste}
					ref={surfaceRef}
					tabIndex={0}
				>
					<img
						alt="remote screen"
						className="max-h-full max-w-full select-none"
						draggable={false}
						onContextMenu={(e) => e.preventDefault()}
						onPointerCancel={onPointerCancel}
						onPointerDown={(e) => {
							surfaceRef.current?.focus()
							onPointerDown(e)
						}}
						onPointerMove={onMove}
						onPointerUp={onPointerUp}
						ref={imgRef}
						style={{ imageRendering: "auto" }}
					/>
				</div>
			</div>
		</div>
	)
}
