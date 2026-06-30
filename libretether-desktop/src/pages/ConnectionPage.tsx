import { writeText } from "@tauri-apps/plugin-clipboard-manager"
import { Copy, Eye, EyeOff, Fingerprint, Network, Save, ScreenShare, Server, Wifi, WifiOff } from "lucide-react"
import { useEffect, useState } from "react"
import { Combobox } from "../components/Combobox"
import { PageHeader } from "../components/PageHeader"
import { Badge, Button, Field, Input } from "../components/ui"
import * as api from "../lib/api"
import { useToast } from "../lib/toast"
import type { ActiveInfo, RdpMode } from "../lib/types"
import { useAsyncAction } from "../lib/useAsyncAction"

const PRESET_MODES: RdpMode[] = ["auto", "freerdp", "remmina", "gnome-connections"]

/** A copyable value. For `secret` values the text is masked behind a reveal
 *  toggle and the copy confirmation doesn't echo the value (it would otherwise
 *  land in the toast and clipboard-history for a high-value credential). */
function CopyRow({ value, secret = false }: { value: string; secret?: boolean }) {
	const toast = useToast()
	const [revealed, setRevealed] = useState(false)
	const masked = secret && !revealed
	const copy = () =>
		writeText(value)
			.then(() => toast.success("Copied", secret ? "Copied to clipboard." : value))
			.catch((e) => toast.error("Copy failed", api.errString(e)))
	return (
		<div className="flex items-center gap-2 rounded-xl border border-border bg-surface-2 px-3.5 py-2.5">
			<code className="flex-1 truncate font-mono text-[0.82rem] text-text">
				{masked ? "•".repeat(Math.min(value.length, 24)) : value}
			</code>
			{secret && (
				<Button
					icon={revealed ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
					onClick={() => setRevealed((r) => !r)}
					size="sm"
					variant="ghost"
				>
					{revealed ? "Hide" : "Show"}
				</Button>
			)}
			<Button icon={<Copy className="h-3.5 w-3.5" />} onClick={copy} size="sm" variant="ghost">
				Copy
			</Button>
		</div>
	)
}

function SectionTitle({ icon, children }: { icon: React.ReactNode; children: React.ReactNode }) {
	return (
		<div className="flex items-center gap-2.5">
			<span className="text-primary dark:text-primary-strong">{icon}</span>
			<h2 className="font-display font-semibold text-text">{children}</h2>
		</div>
	)
}

export function ConnectionPage({ active }: { active: ActiveInfo }) {
	const toast = useToast()
	const saveAction = useAsyncAction()
	const [rdpMode, setRdpMode] = useState<RdpMode>("auto")
	const [rdpCustom, setRdpCustom] = useState("")
	const [terminal, setTerminal] = useState("")

	useEffect(() => {
		api.getSettings()
			.then((s) => {
				setTerminal(s.terminal ?? "")
				const rc = s.rdp_client ?? ""
				if (rc === "" || (PRESET_MODES as string[]).includes(rc)) setRdpMode((rc || "auto") as RdpMode)
				else {
					setRdpMode("custom")
					setRdpCustom(rc)
				}
			})
			.catch((e) => toast.error("Couldn't load settings", api.errString(e)))
	}, [toast])

	const savePrefs = async () => {
		const rdpClient = rdpMode === "custom" ? rdpCustom.trim() || null : rdpMode === "auto" ? null : rdpMode
		const ok = await saveAction.run("Couldn't save", () => api.setSettings(rdpClient, terminal || null))
		if (ok) toast.success("Saved", "Host preferences updated.")
	}

	const ts = active.tailscale
	const kind = active.kind

	return (
		<>
			<PageHeader
				actions={
					<Badge tone="primary">
						<span className="capitalize">{kind.type}</span>
					</Badge>
				}
				eyebrow="Controller"
				subtitle={`How agents reach “${active.name}”. Edit the type from the launch screen.`}
				title="Connection"
			/>

			<div className="min-h-0 flex-1 overflow-y-auto px-7 py-6">
				<div className="mx-auto flex max-w-2xl flex-col gap-4">
					<section className="card flex flex-col gap-3 p-5">
						<SectionTitle icon={<Network className="h-5 w-5" />}>
							Agents reach this controller at
						</SectionTitle>
						{active.reachable_at ? (
							<CopyRow value={active.reachable_at} />
						) : (
							<p className="text-sm text-muted">
								{kind.type === "tailscale"
									? "No tailnet address yet — install Tailscale and sign in on this machine."
									: "No address set. Edit this controller on the launch screen to add an advertise address."}
							</p>
						)}
					</section>

					{kind.type === "tailscale" && (
						<section className="card flex flex-col gap-3 p-5">
							<div className="flex items-center gap-2.5">
								<SectionTitle icon={<Wifi className="h-5 w-5" />}>Tailscale</SectionTitle>
								{ts?.running ? (
									<Badge tone="success">connected</Badge>
								) : ts?.installed ? (
									<Badge tone="warning">installed, not running</Badge>
								) : (
									<Badge tone="danger">
										<WifiOff className="h-3 w-3" /> not found
									</Badge>
								)}
							</div>
							<p className="text-sm text-muted">
								{kind.auth_key
									? "Deploy scripts join the tailnet with the saved auth key — no client login."
									: "No auth key saved; deploy scripts assume the client is already on the tailnet."}
							</p>
						</section>
					)}

					{kind.type === "relay" && (
						<section className="card flex flex-col gap-3 p-5">
							<SectionTitle icon={<Server className="h-5 w-5" />}>Relay secrets</SectionTitle>
							<p className="text-xs text-muted">
								The controller and agents authenticate to{" "}
								<code className="font-mono">libretether-relay</code> with these.
							</p>
							<Field label="Owner secret">
								<CopyRow secret value={kind.owner_secret} />
							</Field>
							<Field label="Agent secret">
								<CopyRow secret value={kind.agent_secret} />
							</Field>
						</section>
					)}

					<section className="card flex flex-col gap-4 p-5">
						<div className="flex flex-wrap items-center gap-2.5">
							<SectionTitle icon={<ScreenShare className="h-5 w-5" />}>Host tools</SectionTitle>
							<span className="text-xs text-subtle">apply to every controller on this machine</span>
						</div>

						<Field hint="Which client the “Connect via RDP” button launches." label="RDP client">
							<Combobox<RdpMode>
								onChange={setRdpMode}
								options={[
									{ label: "Auto-detect", value: "auto" },
									{ label: "FreeRDP", value: "freerdp" },
									{ label: "Remmina", value: "remmina" },
									{ label: "GNOME Connections", value: "gnome-connections" },
									{ label: "Custom command…", value: "custom" }
								]}
								value={rdpMode}
							/>
						</Field>

						{rdpMode === "custom" && (
							<Field hint="Placeholders: {host} {port} {user} {password}" label="Custom RDP command">
								<Input
									className="font-mono"
									onChange={(e) => setRdpCustom(e.target.value)}
									placeholder="e.g. remmina -c rdp://{user}:{password}@{host}:{port}"
									value={rdpCustom}
								/>
							</Field>
						)}

						<Field
							hint="Terminal that “Connect via SSH” opens. Blank = auto-detect; include the run flag, e.g. “xterm -e”."
							label="Terminal (SSH)"
						>
							<Input
								className="font-mono"
								onChange={(e) => setTerminal(e.target.value)}
								placeholder="auto-detect (gnome-terminal --, konsole -e, …)"
								value={terminal}
							/>
						</Field>

						<div className="flex justify-end">
							<Button
								icon={<Save className="h-4 w-4" />}
								loading={saveAction.busy}
								onClick={savePrefs}
								variant="primary"
							>
								Save preferences
							</Button>
						</div>
					</section>

					<section className="card flex items-center gap-4 p-5">
						<div className="grid h-11 w-11 shrink-0 place-items-center rounded-xl bg-surface-2 text-primary dark:text-primary-strong">
							<Fingerprint className="h-5 w-5" />
						</div>
						<div className="min-w-0 flex-1">
							<div className="eyebrow">Identity</div>
							<div className="mt-0.5 text-sm text-muted">
								Controller fingerprint <code className="font-mono text-text">{active.fingerprint}</code>
							</div>
						</div>
					</section>
				</div>
			</div>
		</>
	)
}
