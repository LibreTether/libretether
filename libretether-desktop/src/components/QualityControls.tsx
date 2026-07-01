import { Gauge, SlidersHorizontal } from "lucide-react"
import { useEffect, useRef, useState } from "react"
import type { SessionConfig } from "../lib/types"

/** Bundled quality presets. `display` is preserved from the live config. */
const PRESETS = {
	Auto: { auto: true, bitrate_kbps: 8000, max_fps: 30, scale: 100 },
	High: { auto: false, bitrate_kbps: 12000, max_fps: 30, scale: 100 },
	Low: { auto: false, bitrate_kbps: 2500, max_fps: 20, scale: 50 },
	Medium: { auto: false, bitrate_kbps: 6000, max_fps: 30, scale: 75 }
} satisfies Record<string, Omit<SessionConfig, "display">>

type PresetName = keyof typeof PRESETS

function activePreset(cfg: SessionConfig): PresetName | "Custom" {
	for (const [name, p] of Object.entries(PRESETS) as [PresetName, (typeof PRESETS)[PresetName]][]) {
		if (
			p.scale === cfg.scale &&
			p.bitrate_kbps === cfg.bitrate_kbps &&
			p.max_fps === cfg.max_fps &&
			p.auto === cfg.auto
		) {
			return name
		}
	}
	return "Custom"
}

/** Live quality controls for a running session: one-click presets plus an
 *  Advanced panel of granular sliders. Every change is pushed up via `onChange`
 *  (the overlay forwards it to the agent with `configure_control`). */
export function QualityControls({ value, onChange }: { value: SessionConfig; onChange: (cfg: SessionConfig) => void }) {
	const [open, setOpen] = useState(false)
	const [advanced, setAdvanced] = useState(false)
	const rootRef = useRef<HTMLDivElement>(null)
	const current = activePreset(value)

	// Close when focus/click leaves the control.
	useEffect(() => {
		if (!open) return
		const onDown = (e: MouseEvent) => {
			if (rootRef.current && !rootRef.current.contains(e.target as Node)) setOpen(false)
		}
		window.addEventListener("mousedown", onDown)
		return () => window.removeEventListener("mousedown", onDown)
	}, [open])

	const applyPreset = (name: PresetName) => onChange({ display: value.display, ...PRESETS[name] })
	const setField = (patch: Partial<SessionConfig>) => onChange({ ...value, auto: false, ...patch })

	return (
		<div className="relative" ref={rootRef}>
			<button
				className="flex items-center gap-1.5 rounded-lg border border-white/15 bg-white/10 px-2.5 py-1.5 text-xs font-medium text-white/80 transition hover:bg-white/15"
				onClick={() => setOpen((o) => !o)}
				type="button"
			>
				<Gauge className="h-3.5 w-3.5" />
				<span>
					{current === "Custom"
						? `${value.scale}% · ${(value.bitrate_kbps / 1000).toFixed(1)} Mbps`
						: current}
				</span>
			</button>

			{open && (
				<div className="absolute right-0 top-full z-10 mt-2 w-64 rounded-xl border border-white/10 bg-neutral-900/95 p-3 text-white shadow-2xl backdrop-blur">
					<div className="mb-2 text-[11px] font-semibold uppercase tracking-wide text-white/40">
						Stream quality
					</div>
					<div className="grid grid-cols-2 gap-1.5">
						{(Object.keys(PRESETS) as PresetName[]).map((name) => (
							<button
								className={`rounded-lg px-2 py-1.5 text-xs font-medium transition ${
									current === name
										? "bg-primary text-white"
										: "bg-white/10 text-white/70 hover:bg-white/15"
								}`}
								key={name}
								onClick={() => applyPreset(name)}
								type="button"
							>
								{name}
							</button>
						))}
					</div>

					<button
						className="mt-3 flex w-full items-center gap-1.5 text-[11px] font-medium text-white/45 transition hover:text-white/70"
						onClick={() => setAdvanced((a) => !a)}
						type="button"
					>
						<SlidersHorizontal className="h-3 w-3" />
						Advanced
						<span className="ml-auto">{advanced ? "–" : "+"}</span>
					</button>

					{advanced && (
						<div className="mt-2 space-y-3">
							<Slider
								label="Resolution"
								max={100}
								min={25}
								onChange={(v) => setField({ scale: v })}
								step={5}
								suffix="%"
								value={value.scale}
							/>
							<Slider
								label="Bitrate"
								max={30000}
								min={1000}
								onChange={(v) => setField({ bitrate_kbps: v })}
								step={500}
								suffix=" kbps"
								value={value.bitrate_kbps}
							/>
							<Slider
								label="Frame rate"
								max={60}
								min={5}
								onChange={(v) => setField({ max_fps: v })}
								step={5}
								suffix=" fps"
								value={value.max_fps}
							/>
							<label className="flex items-center gap-2 text-xs text-white/70">
								<input
									checked={value.auto}
									className="accent-primary"
									onChange={(e) => onChange({ ...value, auto: e.target.checked })}
									type="checkbox"
								/>
								Adapt automatically when the link is slow
							</label>
						</div>
					)}
				</div>
			)}
		</div>
	)
}

function Slider({
	label,
	value,
	min,
	max,
	step,
	suffix = "",
	onChange
}: {
	label: string
	value: number
	min: number
	max: number
	step: number
	suffix?: string
	onChange: (v: number) => void
}) {
	return (
		<div>
			<div className="mb-1 flex items-center justify-between text-xs">
				<span className="text-white/60">{label}</span>
				<span className="font-mono text-white/80">
					{value}
					{suffix}
				</span>
			</div>
			<input
				className="h-1 w-full cursor-pointer appearance-none rounded-full bg-white/15 accent-primary"
				max={max}
				min={min}
				onChange={(e) => onChange(Number(e.target.value))}
				step={step}
				type="range"
				value={value}
			/>
		</div>
	)
}
