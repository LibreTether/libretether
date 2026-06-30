import { openUrl } from "@tauri-apps/plugin-opener"
import { Bug, Pencil, Plus, Trash2 } from "lucide-react"
import { useCallback, useEffect, useState } from "react"
import { ControllerForm } from "../components/ControllerForm"
import { useConfirm } from "../components/confirm"
import { RelayConnecting } from "../components/RelayConnecting"
import { Badge, Button, Spinner } from "../components/ui"
import * as api from "../lib/api"
import { CONTROLLER_TYPE_META } from "../lib/meta"
import { useToast } from "../lib/toast"
import type { ActiveInfo, ControllerSummary } from "../lib/types"
import { useAsyncAction } from "../lib/useAsyncAction"

const REPO_URL = "https://github.com/LibreTether/libretether"
const ISSUES_URL = `${REPO_URL}/issues/new`

export function ControllerSelect({ onConnected }: { onConnected: (a: ActiveInfo) => void }) {
	const toast = useToast()
	const confirm = useConfirm()
	const deleteAction = useAsyncAction()
	const [controllers, setControllers] = useState<ControllerSummary[]>([])
	const [loading, setLoading] = useState(true)
	const [form, setForm] = useState<{ existing: ControllerSummary | null } | null>(null)
	const [connecting, setConnecting] = useState<string | null>(null)
	const [connectingRelay, setConnectingRelay] = useState<ControllerSummary | null>(null)

	const reload = useCallback(() => {
		setLoading(true)
		api.listControllers()
			.then(setControllers)
			.catch((e) => toast.error("Couldn't load controllers", api.errString(e)))
			.finally(() => setLoading(false))
	}, [toast])

	useEffect(() => reload(), [reload])

	const connect = async (c: ControllerSummary) => {
		// Relay controllers dial out — show the connecting screen and only enter
		// once the relay accepts. Direct/Tailscale just bind a listener locally.
		if (c.kind.type === "relay") {
			setConnectingRelay(c)
			return
		}
		setConnecting(c.id)
		try {
			onConnected(await api.selectController(c.id))
		} catch (e) {
			toast.error("Couldn't connect", api.errString(e))
			setConnecting(null)
		}
	}

	const remove = async (c: ControllerSummary) => {
		const ok = await confirm({
			confirmLabel: "Delete",
			message: `Delete “${c.name}” and its ${c.machine_count} machine(s)? This cannot be undone.`,
			title: "Delete controller",
			tone: "danger"
		})
		if (!ok) return
		await deleteAction.run("Couldn't delete", async () => {
			await api.deleteController(c.id)
			reload()
		})
	}

	if (connectingRelay) {
		return (
			<RelayConnecting
				controller={connectingRelay}
				onCancel={() => setConnectingRelay(null)}
				onConnected={onConnected}
			/>
		)
	}

	return (
		<div className="flex h-screen flex-col overflow-hidden">
			<div className="drag h-9 shrink-0" />
			<div className="flex min-h-0 flex-1 items-center justify-center overflow-y-auto p-6">
				<div className="flex w-full max-w-lg flex-col gap-5">
					<div className="flex flex-col items-center gap-2 text-center">
						<img alt="" className="h-12 w-12 rounded-xl" src="/libretether.png" />
						<h1 className="text-xl font-bold text-text">Choose a controller</h1>
						<p className="text-sm text-muted">
							Each controller manages its own set of machines. Only one runs at a time.
						</p>
					</div>

					{loading ? (
						<div className="flex justify-center py-10">
							<Spinner className="h-6 w-6" />
						</div>
					) : (
						<div className="flex flex-col gap-2.5">
							{controllers.map((c) => {
								const meta = CONTROLLER_TYPE_META[c.kind.type]
								const Icon = meta.icon
								return (
									<div
										className="card group flex items-center gap-3.5 p-4 transition hover:border-primary/50"
										key={c.id}
									>
										<div className="grid h-10 w-10 shrink-0 place-items-center rounded-xl bg-surface-2 text-primary dark:text-primary-strong">
											<Icon className="h-5 w-5" />
										</div>
										<div className="min-w-0 flex-1">
											<div className="flex items-center gap-2">
												<span className="truncate font-semibold text-text">{c.name}</span>
												<Badge tone="neutral">{meta.label}</Badge>
											</div>
											<div className="truncate text-xs text-subtle">
												{c.machine_count} machine{c.machine_count === 1 ? "" : "s"} ·{" "}
												{c.fingerprint}
											</div>
										</div>
										<button
											aria-label="Edit"
											className="no-drag rounded-lg p-2 text-subtle opacity-0 transition hover:bg-surface-3 hover:text-text group-hover:opacity-100"
											onClick={() => setForm({ existing: c })}
										>
											<Pencil className="h-4 w-4" />
										</button>
										<button
											aria-label="Delete"
											className="no-drag rounded-lg p-2 text-subtle opacity-0 transition hover:bg-danger-soft hover:text-danger group-hover:opacity-100"
											onClick={() => remove(c)}
										>
											<Trash2 className="h-4 w-4" />
										</button>
										<Button
											loading={connecting === c.id}
											onClick={() => connect(c)}
											variant="primary"
										>
											Connect
										</Button>
									</div>
								)
							})}

							<Button
								className="justify-center border border-dashed border-border py-3"
								icon={<Plus className="h-4 w-4" />}
								onClick={() => setForm({ existing: null })}
								variant="ghost"
							>
								New controller
							</Button>
						</div>
					)}
				</div>
			</div>

			<footer className="flex shrink-0 items-center justify-center gap-2 pb-4 text-xs text-subtle">
				<span>v{__APP_VERSION__}</span>
				<span aria-hidden="true">·</span>
				<button
					className="no-drag inline-flex items-center gap-1.5 rounded transition hover:text-text"
					onClick={() => openUrl(REPO_URL)}
					type="button"
				>
					<svg aria-hidden="true" className="h-3.5 w-3.5" fill="currentColor" viewBox="0 0 16 16">
						<path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82a7.65 7.65 0 0 1 2-.27c.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0 0 16 8c0-4.42-3.58-8-8-8z" />
					</svg>
					GitHub
				</button>
				<span aria-hidden="true">·</span>
				<button
					className="no-drag inline-flex items-center gap-1.5 rounded transition hover:text-text"
					onClick={() => openUrl(ISSUES_URL)}
					type="button"
				>
					<Bug className="h-3.5 w-3.5" />
					Report issue
				</button>
			</footer>

			{form && (
				<ControllerForm
					existing={form.existing}
					onClose={() => setForm(null)}
					onSaved={() => {
						setForm(null)
						reload()
					}}
				/>
			)}
		</div>
	)
}
