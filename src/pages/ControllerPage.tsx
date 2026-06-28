import { writeText } from "@tauri-apps/plugin-clipboard-manager"
import { Copy, Fingerprint, KeyRound, Network, RefreshCw, Save, Wifi, WifiOff } from "lucide-react"
import { useCallback, useEffect, useState } from "react"
import { Badge, Button, Field, Input } from "../components/ui"
import * as api from "../lib/api"
import { useToast } from "../lib/toast"
import type { ControllerInfo } from "../lib/types"

export function ControllerPage() {
	const toast = useToast()
	const [info, setInfo] = useState<ControllerInfo | null>(null)
	const [busy, setBusy] = useState(true)
	const [advertise, setAdvertise] = useState("")
	const [authKey, setAuthKey] = useState("")
	const [saving, setSaving] = useState(false)

	const load = useCallback(() => {
		setBusy(true)
		api.controllerInfo()
			.then((i) => {
				setInfo(i)
				setAdvertise(i.advertise_addr ?? "")
				setAuthKey(i.tailscale_auth_key ?? "")
			})
			.catch((e) => toast.error("Couldn't load controller info", api.errString(e)))
			.finally(() => setBusy(false))
	}, [toast])

	useEffect(() => {
		load()
	}, [load])

	const save = async () => {
		setSaving(true)
		try {
			await api.setControllerSettings(advertise || null, authKey || null)
			toast.success("Saved", "New deploy scripts will use these settings.")
			load()
		} catch (e) {
			toast.error("Couldn't save", api.errString(e))
		} finally {
			setSaving(false)
		}
	}

	const ts = info?.tailscale
	const dialAddress = advertise || (ts?.address ? `${ts.address}:${info?.listen_port}` : null)

	return (
		<>
			<header className="drag flex items-center justify-between border-b border-border px-7 py-5">
				<div>
					<h1 className="text-xl font-bold text-text">Controller</h1>
					<p className="text-sm text-muted">How agents reach this machine, and its identity.</p>
				</div>
				<Button icon={<RefreshCw className="h-4 w-4" />} loading={busy} onClick={load} variant="outline">
					Refresh
				</Button>
			</header>

			<div className="min-h-0 flex-1 overflow-y-auto px-7 py-6">
				<div className="mx-auto flex max-w-2xl flex-col gap-4">
					<section className="card flex flex-col gap-4 p-5">
						<div className="flex items-center gap-2.5">
							<Network className="h-5 w-5 text-primary dark:text-primary-strong" />
							<h2 className="font-semibold text-text">Connection</h2>
						</div>

						<Field
							hint="What agents dial. A tailnet name/IP, a LAN address, or a public host:port. Leave blank to auto-use the Tailscale address below."
							label="Advertise address"
						>
							<Input
								onChange={(e) => setAdvertise(e.target.value)}
								placeholder={
									ts?.address
										? `${ts.address}:${info?.listen_port}`
										: "e.g. 100.x.y.z:47600 or my-host:47600"
								}
								value={advertise}
							/>
						</Field>

						<Field
							hint="Optional. A Tailscale pre-auth key so clients join your tailnet without logging in. Leave blank for direct/LAN connections (no Tailscale)."
							label="Tailscale auth key"
						>
							<div className="flex items-center gap-2">
								<KeyRound className="h-4 w-4 shrink-0 text-subtle" />
								<Input
									onChange={(e) => setAuthKey(e.target.value)}
									placeholder="tskey-auth-…  (generated in your Tailscale admin console)"
									type="password"
									value={authKey}
								/>
							</div>
						</Field>

						<div className="flex items-center justify-between">
							<p className="text-xs text-subtle">
								{authKey
									? "Mode: Tailscale — deploy scripts join the tailnet with this key, no client login."
									: "Mode: direct — clients must already be able to reach the advertise address."}
							</p>
							<Button
								icon={<Save className="h-4 w-4" />}
								loading={saving}
								onClick={save}
								variant="primary"
							>
								Save
							</Button>
						</div>
					</section>

					<section className="card p-5">
						<div className="mb-3 flex items-center gap-2.5">
							<Wifi className="h-5 w-5 text-primary dark:text-primary-strong" />
							<h2 className="font-semibold text-text">Tailscale status</h2>
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
						{dialAddress ? (
							<div className="flex items-center gap-2 rounded-xl border border-border bg-surface-2 px-3.5 py-2.5">
								<code className="flex-1 truncate text-text">{dialAddress}</code>
								<Button
									icon={<Copy className="h-3.5 w-3.5" />}
									onClick={() =>
										writeText(dialAddress).then(() => toast.success("Copied", dialAddress))
									}
									size="sm"
									variant="ghost"
								>
									Copy
								</Button>
							</div>
						) : (
							<p className="text-sm text-muted">
								No address yet. Set an advertise address above, or install Tailscale and sign in.
							</p>
						)}
					</section>

					<section className="card flex items-center gap-4 p-5">
						<div className="grid h-11 w-11 place-items-center rounded-xl bg-surface-2 text-primary dark:text-primary-strong">
							<Fingerprint className="h-5 w-5" />
						</div>
						<div className="flex-1">
							<div className="text-sm font-semibold text-text">Identity & port</div>
							<div className="text-sm text-muted">
								Fingerprint <code className="text-text">{info?.fingerprint ?? "—"}</code> · QUIC udp/
								<code className="text-text">{info?.listen_port ?? "—"}</code>
							</div>
						</div>
					</section>
				</div>
			</div>
		</>
	)
}
