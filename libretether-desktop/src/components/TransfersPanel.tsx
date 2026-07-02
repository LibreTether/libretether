import { ArrowLeftRight, Download, Pause, Play, Trash2, Upload } from "lucide-react"
import * as api from "../lib/api"
import { formatBytes } from "../lib/format"
import { useToast } from "../lib/toast"
import type { ClientDto, TransferItem, TransferProgress, TransferStatus } from "../lib/types"
import { useTransfers } from "../lib/useTransfers"
import { Badge, Button, Drawer, EmptyState } from "./ui"

const STATUS_TONE: Record<TransferStatus, "neutral" | "primary" | "warning" | "success" | "danger"> = {
	active: "primary",
	done: "success",
	error: "danger",
	paused: "warning",
	queued: "neutral"
}

const STATUS_LABEL: Record<TransferStatus, string> = {
	active: "transferring",
	done: "done",
	error: "failed",
	paused: "paused",
	queued: "queued"
}

/** The live transfer queue, as a right-side drawer: one row per transfer with a
 *  progress bar and pause/resume/remove controls. Progress arrives live over events;
 *  the list refreshes on status changes. */
export function TransfersPanel({
	open,
	onClose,
	clients
}: {
	open: boolean
	onClose: () => void
	clients: ClientDto[]
}) {
	const { transfers, progress } = useTransfers()
	const toast = useToast()

	const machineName = (id: string) => clients.find((c) => c.id === id)?.name ?? "machine"

	const act = (fn: Promise<void>, err: string) => {
		fn.catch((e) => toast.error(err, api.errString(e)))
	}

	return (
		<Drawer
			icon={<ArrowLeftRight className="h-5 w-5" />}
			onClose={onClose}
			open={open}
			size="lg"
			subtitle={transfers.length > 0 ? `${transfers.length} in queue` : undefined}
			title="Transfers"
		>
			{transfers.length === 0 ? (
				<EmptyState
					description="Downloads and uploads you start will show here with live progress. Open a machine's Files action to begin."
					icon={<ArrowLeftRight className="h-6 w-6" />}
					title="No transfers yet"
				/>
			) : (
				<ul className="flex flex-col gap-2.5">
					{transfers.map((t) => (
						<TransferRow
							key={t.id}
							live={progress[t.id]}
							machine={machineName(t.client_id)}
							onPause={() => act(api.pauseTransfer(t.id), "Couldn't pause")}
							onRemove={() => act(api.removeTransfer(t.id), "Couldn't remove")}
							onResume={() => act(api.resumeTransfer(t.id), "Couldn't resume")}
							t={t}
						/>
					))}
				</ul>
			)}
		</Drawer>
	)
}

function TransferRow({
	t,
	live,
	machine,
	onPause,
	onResume,
	onRemove
}: {
	t: TransferItem
	live?: TransferProgress
	machine: string
	onPause: () => void
	onResume: () => void
	onRemove: () => void
}) {
	// Prefer live event figures over the persisted (file-boundary) hint.
	const bytesDone = live?.bytes_done ?? t.bytes_done
	const totalBytes = live?.total_bytes || t.total_bytes
	const filesDone = live?.files_done ?? t.files_done
	const totalFiles = live?.total_files || t.total_files
	const pct =
		t.status === "done" ? 100 : totalBytes > 0 ? Math.min(100, Math.floor((bytesDone / totalBytes) * 100)) : 0
	const running = t.status === "active" || t.status === "queued"

	return (
		<li className="rounded-xl border border-border bg-surface-2/40 px-3.5 py-3">
			<div className="flex items-center gap-2">
				{t.direction === "download" ? (
					<Download className="h-4 w-4 shrink-0 text-accent" />
				) : (
					<Upload className="h-4 w-4 shrink-0 text-primary dark:text-primary-strong" />
				)}
				<span className="min-w-0 flex-1 truncate text-sm font-medium text-text" title={t.name}>
					{t.name}
				</span>
				<Badge tone={STATUS_TONE[t.status]}>{STATUS_LABEL[t.status]}</Badge>
			</div>

			<div className="mt-1 text-[0.72rem] text-subtle">
				{t.direction === "download" ? "from" : "to"} {machine}
				{totalFiles > 1 && ` · ${filesDone}/${totalFiles} files`}
			</div>

			<div className="mt-2 h-1.5 overflow-hidden rounded-full bg-surface-3">
				<div
					className={
						t.status === "error" ? "h-full bg-danger" : "h-full bg-primary transition-[width] duration-200"
					}
					style={{ width: `${t.status === "error" ? 100 : pct}%` }}
				/>
			</div>

			<div className="mt-1.5 flex items-center gap-2">
				<span className="text-[0.72rem] text-subtle">
					{t.status === "error" && t.error ? (
						<span className="text-danger">{t.error}</span>
					) : totalBytes > 0 ? (
						`${formatBytes(bytesDone)} / ${formatBytes(totalBytes)} · ${pct}%`
					) : (
						"preparing…"
					)}
				</span>
				<div className="ml-auto flex items-center gap-1.5">
					{running ? (
						<Button icon={<Pause className="h-3.5 w-3.5" />} onClick={onPause} size="sm" variant="outline">
							Pause
						</Button>
					) : t.status === "paused" || t.status === "error" ? (
						<Button icon={<Play className="h-3.5 w-3.5" />} onClick={onResume} size="sm" variant="outline">
							Resume
						</Button>
					) : null}
					<Button
						aria-label="Remove"
						icon={<Trash2 className="h-3.5 w-3.5" />}
						onClick={onRemove}
						size="icon-sm"
						variant="ghost"
					/>
				</div>
			</div>
		</li>
	)
}
