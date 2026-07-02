import { Fingerprint, Lock, Network, Save, ScreenShare, Server, ShieldCheck, Wifi, WifiOff } from "lucide-react"
import { useEffect, useState } from "react"
import { Combobox } from "../components/Combobox"
import { CopyRow } from "../components/CopyRow"
import { PageHeader } from "../components/PageHeader"
import { Badge, Button, Field, Input } from "../components/ui"
import * as api from "../lib/api"
import { useToast } from "../lib/toast"
import type { ActiveInfo, RdpMode } from "../lib/types"
import { useAsyncAction } from "../lib/useAsyncAction"

const PRESET_MODES: RdpMode[] = ["auto", "freerdp", "remmina", "gnome-connections"]

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
	const [compress, setCompress] = useState(true)

	useEffect(() => {
		api.getSettings()
			.then((s) => {
				setTerminal(s.terminal ?? "")
				setCompress(s.compress_transfers)
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
		const ok = await saveAction.run("Couldn't save", () => api.setSettings(rdpClient, terminal || null, compress))
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

						<label className="flex items-start gap-2.5">
							<input
								checked={compress}
								className="no-drag mt-0.5 h-4 w-4 shrink-0 accent-[var(--primary)]"
								onChange={(e) => setCompress(e.target.checked)}
								type="checkbox"
							/>
							<span className="flex flex-col gap-0.5">
								<span className="text-xs font-semibold text-muted">Compress file transfers</span>
								<span className="text-[0.72rem] leading-relaxed text-subtle">
									Adaptive zstd — compresses each chunk when it helps and skips already-compressed
									files (video, archives, images). Turn off to save CPU on fast local links.
								</span>
							</span>
						</label>

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

					<section className="card flex flex-col gap-4 p-5">
						<SectionTitle icon={<ShieldCheck className="h-5 w-5" />}>Security</SectionTitle>
						<div className="flex flex-wrap gap-1.5">
							<Badge className="gap-1" tone="success">
								<Lock className="h-3 w-3" /> Encrypted (QUIC · TLS 1.3)
							</Badge>
							<Badge className="gap-1" tone="success">
								<ShieldCheck className="h-3 w-3" /> Mutual Ed25519 auth
							</Badge>
							<Badge tone="neutral">Protocol v{active.protocol_version}</Badge>
						</div>
						<p className="text-sm text-muted">
							Every machine authenticates this controller against the key below (the{" "}
							<code className="font-mono text-text">controller_key</code> baked into its deploy command),
							and the controller verifies each agent's signature against the key it pinned at enrollment.
							It's mutual, and there's no trust-on-first-use.
						</p>
						<Field label="Controller identity key (agents pin this)">
							<CopyRow value={active.public_key} />
						</Field>
						<div className="flex items-center gap-2 text-sm text-muted">
							<Fingerprint className="h-4 w-4 text-primary dark:text-primary-strong" />
							Fingerprint <code className="font-mono text-text">{active.fingerprint}</code>
						</div>
					</section>
				</div>
			</div>
		</>
	)
}
