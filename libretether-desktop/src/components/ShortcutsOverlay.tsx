import { MOD_LABEL } from "../lib/hotkeys"
import { Kbd, Modal } from "./ui"

interface Shortcut {
	keys: string[]
	label: string
}

const GROUPS: { name: string; items: Shortcut[] }[] = [
	{
		items: [
			{ keys: [MOD_LABEL, "K"], label: "Command palette" },
			{ keys: ["?"], label: "Keyboard shortcuts" },
			{ keys: ["Esc"], label: "Close panel or dialog" }
		],
		name: "General"
	},
	{
		items: [
			{ keys: ["1"], label: "Go to Machines" },
			{ keys: ["2"], label: "Go to Connection" },
			{ keys: ["3"], label: "Go to Logs" }
		],
		name: "Navigate"
	},
	{
		items: [
			{ keys: ["N"], label: "Add a machine" },
			{ keys: ["/"], label: "Search the list" },
			{ keys: ["J", "K"], label: "Move selection" },
			{ keys: ["↵"], label: "Take control" },
			{ keys: ["S"], label: "Connect via SSH" },
			{ keys: ["R"], label: "Connect via RDP" },
			{ keys: ["D"], label: "Open details" }
		],
		name: "Machines"
	}
]

export function ShortcutsOverlay({ open, onClose }: { open: boolean; onClose: () => void }) {
	return (
		<Modal onClose={onClose} open={open} size="md" title="Keyboard shortcuts">
			<div className="grid gap-x-8 gap-y-5 sm:grid-cols-2">
				{GROUPS.map((g) => (
					<section key={g.name}>
						<div className="eyebrow mb-2">{g.name}</div>
						<ul className="flex flex-col gap-2">
							{g.items.map((s) => (
								<li className="flex items-center justify-between gap-3" key={s.label}>
									<span className="text-sm text-muted">{s.label}</span>
									<span className="flex shrink-0 gap-1">
										{s.keys.map((k) => (
											<Kbd key={k}>{k}</Kbd>
										))}
									</span>
								</li>
							))}
						</ul>
					</section>
				))}
			</div>
		</Modal>
	)
}
