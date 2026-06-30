import { useEffect, useRef } from "react"

/** True on macOS, where the platform modifier is ⌘ rather than Ctrl. Read once
 *  from the UA — Tauri windows don't change platform at runtime. */
export const IS_MAC = typeof navigator !== "undefined" && /mac/i.test(navigator.platform || navigator.userAgent)

/** The platform modifier symbol, for rendering shortcut hints. */
export const MOD_LABEL = IS_MAC ? "⌘" : "Ctrl"

export interface Hotkey {
	/** Combo in the form `mod+k`, `shift+/`, `g`, `Escape`, `ArrowDown`. `mod` is
	 *  ⌘ on macOS and Ctrl elsewhere. The final segment matches `KeyboardEvent.key`
	 *  case-insensitively. */
	combo: string
	handler: (e: KeyboardEvent) => void
	/** Fire even while a text field is focused (default false — most shortcuts
	 *  should yield to typing). */
	allowInInput?: boolean
}

function isTypingTarget(el: EventTarget | null): boolean {
	const node = el as HTMLElement | null
	if (!node?.tagName) return false
	const tag = node.tagName
	return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || node.isContentEditable
}

function matches(combo: string, e: KeyboardEvent): boolean {
	const parts = combo.toLowerCase().split("+")
	const key = parts[parts.length - 1]
	const wantMod = parts.includes("mod")
	const wantShift = parts.includes("shift")
	const wantAlt = parts.includes("alt")
	const haveMod = e.metaKey || e.ctrlKey
	if (wantMod !== haveMod) return false
	if (wantAlt !== e.altKey) return false
	// Shift is implied by some printable keys (e.g. "?"), so only enforce it when
	// the combo asked for it; never reject an unrequested shift for symbol keys.
	if (wantShift && !e.shiftKey) return false
	return e.key.toLowerCase() === key
}

/** Register global keyboard shortcuts for as long as the component is mounted and
 *  `enabled`. Handlers are kept in a ref so passing a fresh array each render
 *  doesn't rebind the listener (and doesn't trip exhaustive-deps). */
export function useHotkeys(hotkeys: Hotkey[], enabled = true) {
	const ref = useRef(hotkeys)
	ref.current = hotkeys

	useEffect(() => {
		if (!enabled) return
		const onKeyDown = (e: KeyboardEvent) => {
			const typing = isTypingTarget(e.target)
			for (const hk of ref.current) {
				if (typing && !hk.allowInInput) continue
				if (matches(hk.combo, e)) {
					e.preventDefault()
					hk.handler(e)
					return
				}
			}
		}
		window.addEventListener("keydown", onKeyDown)
		return () => window.removeEventListener("keydown", onKeyDown)
	}, [enabled])
}
