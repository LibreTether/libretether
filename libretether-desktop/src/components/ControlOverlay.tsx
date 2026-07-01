import { Eye, Keyboard, Loader2, MousePointer2, Power } from "lucide-react"
import { useCallback, useEffect, useRef, useState } from "react"
import * as api from "../lib/api"
import type { ClientDto, InputEvent, MouseButton, SessionConfig, SessionMeta } from "../lib/types"
import { avcCodecFromKeyframe, parseFrame } from "../lib/videoFrame"
import { QualityControls } from "./QualityControls"

const BUTTONS: Record<number, MouseButton> = { 0: "left", 1: "middle", 2: "right" }

// The session opens in adaptive mode at native resolution; the controller can
// retune it live from the QualityControls menu.
const INITIAL_CONFIG: SessionConfig = { auto: true, bitrate_kbps: 8000, display: 0, max_fps: 30, scale: 100 }

export function ControlOverlay({
	client,
	onClose,
	readOnly = false
}: {
	client: ClientDto
	onClose: () => void
	/** Watch mode: receive and render the stream but forward no input to the agent. */
	readOnly?: boolean
}) {
	const canvasRef = useRef<HTMLCanvasElement>(null)
	const surfaceRef = useRef<HTMLDivElement>(null)
	const [meta, setMeta] = useState<SessionMeta | null>(null)
	const [error, setError] = useState<string | null>(null)
	const [config, setConfig] = useState<SessionConfig>(INITIAL_CONFIG)
	// Count frames in a ref and surface a frames-per-second readout ~1 Hz, so a
	// high-rate stream doesn't re-render the whole overlay on every frame (the
	// canvas is drawn imperatively). `prevFramesRef` holds the last tick's total so
	// the interval can report the delta (an fps), not an ever-growing count.
	const framesRef = useRef(0)
	const prevFramesRef = useRef(0)
	const [fps, setFps] = useState(0)

	// Frames are decoded strictly in arrival order: a P-frame builds on the frames
	// before it, so reordering or dropping one corrupts the picture until the next
	// keyframe. WebCodecs preserves submission order and the agent delivers frames in
	// order over the channel, so we feed them straight through — no queue needed.
	const decoderRef = useRef<VideoDecoder | null>(null)
	// The codec string is derived from the first keyframe's SPS; track it so a
	// profile/level change reconfigures the decoder.
	const codecRef = useRef<string | null>(null)
	// EncodedVideoChunk wants a monotonic timestamp; the value is arbitrary for
	// in-order baseline H.264 (no B-frames), so a simple counter suffices.
	const tsRef = useRef(0)
	// Latest config, read by the decoder's error-recovery path without making the
	// session effect depend on it.
	const configRef = useRef(config)
	const aliveRef = useRef(true)

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
			// Watch mode is read-only: never forward input to the agent. This is the
			// single choke point every handler routes through, so guarding it here is
			// enough to guarantee no input leaves the controller.
			if (readOnly) return
			api.sendInput(client.id, event).catch(() => {
				/* session closing — ignore */
			})
		},
		[client.id, readOnly]
	)

	// Draw a decoded frame onto the canvas, sizing the canvas to match it. The
	// decoder owns scaling/color conversion on the GPU, so this is a single blit.
	const onDecoded = useCallback((frame: VideoFrame) => {
		const canvas = canvasRef.current
		if (!canvas || !aliveRef.current) {
			frame.close()
			return
		}
		if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
			canvas.width = frame.displayWidth
			canvas.height = frame.displayHeight
		}
		const ctx = canvas.getContext("2d")
		if (ctx) ctx.drawImage(frame, 0, 0)
		frame.close()
	}, [])

	// Drop the decoder and ask the agent for a fresh keyframe to rebuild from. A
	// corrupt or missed frame can't be patched, only restarted from an IDR;
	// `configure_control` re-sends the live config, which forces one agent-side.
	const resetDecoder = useCallback(() => {
		const dec = decoderRef.current
		decoderRef.current = null
		codecRef.current = null
		if (dec && dec.state !== "closed") {
			try {
				dec.close()
			} catch {
				/* already gone */
			}
		}
		api.configureControl(client.id, configRef.current).catch(() => {})
	}, [client.id])

	// Feed one frame to the decoder. A keyframe (re)configures it from the in-band
	// SPS; deltas arriving before the decoder is configured are dropped (the stream
	// opens with a keyframe, so that only happens briefly during error recovery).
	const handleFrame = useCallback(
		(buf: ArrayBuffer) => {
			if (!aliveRef.current) return
			const frame = parseFrame(buf)
			if (frame.key) {
				const codec = avcCodecFromKeyframe(frame.data) ?? "avc1.42e01f"
				if (!decoderRef.current || codecRef.current !== codec) {
					if (decoderRef.current && decoderRef.current.state !== "closed") {
						try {
							decoderRef.current.close()
						} catch {
							/* already gone */
						}
					}
					const dec = new VideoDecoder({
						error: () => aliveRef.current && resetDecoder(),
						output: onDecoded
					})
					dec.configure({ codec, optimizeForLatency: true })
					decoderRef.current = dec
					codecRef.current = codec
				}
			}
			const dec = decoderRef.current
			if (dec?.state !== "configured") return
			try {
				dec.decode(
					new EncodedVideoChunk({
						data: frame.data,
						timestamp: tsRef.current,
						type: frame.key ? "key" : "delta"
					})
				)
				tsRef.current += 1000
			} catch {
				if (aliveRef.current) resetDecoder()
			}
		},
		[onDecoded, resetDecoder]
	)

	// Release everything currently held, then forget it. Safe to call repeatedly.
	const releaseAll = useCallback(() => {
		for (const code of pressedKeys.current) send({ code, pressed: false, t: "key" })
		pressedKeys.current.clear()
		for (const button of pressedButtons.current) send({ button, pressed: false, t: "mouse_button" })
		pressedButtons.current.clear()
	}, [send])

	// Push a live quality change to the agent (no session restart).
	const applyConfig = useCallback(
		(next: SessionConfig) => {
			setConfig(next)
			api.configureControl(client.id, next).catch(() => {})
		},
		[client.id]
	)

	// Subscribe to session events, then start the session.
	useEffect(() => {
		aliveRef.current = true
		const unlisteners: Promise<() => void>[] = [
			api.onSessionMeta(client.id, (m) => aliveRef.current && setMeta(m)),
			api.onSessionError(client.id, (msg) => aliveRef.current && setError(msg)),
			api.onSessionClosed(client.id, () => aliveRef.current && setError((prev) => prev ?? "The session ended."))
		]

		// Defer the start by a tick so React StrictMode's throwaway mount (which
		// unmounts immediately) never actually opens a session — avoids a second
		// portal consent dialog on the client.
		const startTimer = setTimeout(() => {
			if (!aliveRef.current) return
			api.startControl(client.id, INITIAL_CONFIG, (buf) => {
				if (!aliveRef.current) return
				framesRef.current += 1
				handleFrame(buf)
			}).catch((e) => aliveRef.current && setError(api.errString(e)))
		}, 0)
		surfaceRef.current?.focus()

		return () => {
			aliveRef.current = false
			clearTimeout(startTimer)
			const dec = decoderRef.current
			decoderRef.current = null
			if (dec && dec.state !== "closed") {
				try {
					dec.close()
				} catch {
					/* already gone */
				}
			}
			// Flush any held keys/buttons *before* tearing the session down so the
			// releases still reach the agent (the agent also releases on stop as a
			// backstop, but this covers both backends and is immediate).
			releaseAll()
			api.stopControl(client.id).catch(() => {})
			for (const u of unlisteners) u.then((fn) => fn())
		}
	}, [client.id, releaseAll, handleFrame])

	// Report fps ~1 Hz as the delta since the last tick (see above).
	useEffect(() => {
		const t = window.setInterval(() => {
			setFps(framesRef.current - prevFramesRef.current)
			prevFramesRef.current = framesRef.current
		}, 1000)
		return () => window.clearInterval(t)
	}, [])

	// Keep the decoder's view of the live config current for error recovery.
	useEffect(() => {
		configRef.current = config
	}, [config])

	// Keyboard is handled at the window level (not on the focused surface) so that
	// control doesn't silently die the moment focus moves to the header, a button,
	// or a toast. Escape closes the overlay; everything else is forwarded.
	useEffect(() => {
		const onKeyDown = (e: KeyboardEvent) => {
			// A double-tap of Escape (two presses within 500ms) exits the overlay; a
			// single Escape falls through and is forwarded to the remote like any key.
			// Esc-Esc works in watch mode too, so this runs before the read-only gate.
			if (e.key === "Escape" && !e.repeat) {
				if (e.timeStamp - escapeAt.current < 500) {
					onClose()
					return
				}
				escapeAt.current = e.timeStamp
			}
			// Watch mode forwards nothing, so don't capture the keyboard — let every
			// other key reach the host normally.
			if (readOnly) return
			e.preventDefault()
			pressedKeys.current.add(e.code)
			send({ code: e.code, pressed: true, t: "key" })
		}
		const onKeyUp = (e: KeyboardEvent) => {
			if (readOnly) return
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
	}, [send, releaseAll, onClose, readOnly])

	// Forward wheel scroll to the agent and stop it from scrolling/zooming the
	// host webview. React's synthetic `onWheel` is passive (preventDefault is a
	// no-op there), so attach a non-passive native listener.
	useEffect(() => {
		const el = surfaceRef.current
		// Watch mode forwards no scroll, so don't intercept the wheel at all.
		if (!el || readOnly) return
		const onWheelNative = (e: WheelEvent) => {
			e.preventDefault()
			const dy = Math.round(e.deltaY / 100) || Math.sign(e.deltaY)
			const dx = Math.round(e.deltaX / 100) || Math.sign(e.deltaX)
			if (dx || dy) send({ dx, dy, t: "mouse_scroll" })
		}
		el.addEventListener("wheel", onWheelNative, { passive: false })
		return () => el.removeEventListener("wheel", onWheelNative)
	}, [send, readOnly])

	const norm = (e: React.PointerEvent | React.MouseEvent): { x: number; y: number } | null => {
		const el = canvasRef.current
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
			<div className="no-drag flex items-center gap-3 border-b border-white/10 bg-black px-4 py-2.5 text-white">
				<span className="signal-live h-2.5 w-2.5 shrink-0 rounded-full bg-success" />
				{readOnly ? (
					<Eye className="h-4 w-4 text-primary-strong" />
				) : (
					<MousePointer2 className="h-4 w-4 text-primary-strong" />
				)}
				<span className="font-display font-semibold">{client.name}</span>
				{readOnly && (
					<span className="rounded-md bg-white/10 px-1.5 py-0.5 font-mono text-[0.65rem] uppercase tracking-wide text-white/60">
						read-only
					</span>
				)}
				<span className="font-mono text-xs text-white/45">
					{meta ? `${meta.width}×${meta.height}` : "connecting…"}
					{fps > 0 && ` · ${fps} fps`}
					{meta && ` · ${meta.capture} · ${meta.encoder}`}
				</span>
				<span className="ml-auto flex items-center gap-2 text-xs text-white/45">
					{readOnly ? <Eye className="h-3.5 w-3.5" /> : <Keyboard className="h-3.5 w-3.5" />}
					<span>{readOnly ? "watching — input disabled" : "type to control"}</span>
					<span className="text-white/25">·</span>
					<span className="inline-flex items-center gap-1">
						<kbd className="kbd h-5 min-w-5 border-white/20 bg-white/10 text-white/70">Esc</kbd>
						<kbd className="kbd h-5 min-w-5 border-white/20 bg-white/10 text-white/70">Esc</kbd>
						to exit
					</span>
				</span>
				<QualityControls onChange={applyConfig} value={config} />
				<button
					className="flex items-center gap-1.5 rounded-lg bg-danger/90 px-3 py-1.5 text-xs font-semibold text-white transition hover:bg-danger"
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
					<canvas
						className="max-h-full max-w-full select-none"
						onContextMenu={(e) => e.preventDefault()}
						onPointerCancel={onPointerCancel}
						onPointerDown={(e) => {
							surfaceRef.current?.focus()
							onPointerDown(e)
						}}
						onPointerMove={onMove}
						onPointerUp={onPointerUp}
						ref={canvasRef}
						style={{ imageRendering: "auto" }}
					/>
				</div>
			</div>
		</div>
	)
}
