import { ArrowRight, Download, Monitor, Server, Upload } from "lucide-react"
import { useCallback, useState } from "react"
import * as api from "../lib/api"
import { cn } from "../lib/cn"
import { useToast } from "../lib/toast"
import type { ClientDto, TransferDirection } from "../lib/types"
import { useAsyncAction } from "../lib/useAsyncAction"
import { FileBrowser, type SelectedEntry } from "./FileBrowser"
import { Button, Modal } from "./ui"

/**
 * Two-pane file-transfer picker for one machine. A direction toggle chooses which side
 * is the source (multi-select) and which is the destination folder; "Transfer" enqueues
 * one queue item per selected file/folder. Progress then shows in the Transfers panel.
 */
export function TransferDialog({
	client,
	onClose,
	onQueued
}: {
	client: ClientDto
	onClose: () => void
	/** Called after transfers are successfully enqueued (so the app can reveal the queue). */
	onQueued?: () => void
}) {
	const toast = useToast()
	const action = useAsyncAction()
	const [direction, setDirection] = useState<TransferDirection>("download")
	const [selected, setSelected] = useState<SelectedEntry[]>([])
	const [destDir, setDestDir] = useState<string | null>(null)

	// Stable listers so the FileBrowser panes don't reload on every render.
	const loadRemote = useCallback((path: string | null) => api.browseRemote(client.id, path), [client.id])
	const loadLocal = useCallback((path: string | null) => api.browseLocal(path), [])

	const setDir = (d: TransferDirection) => {
		setDirection(d)
		setSelected([])
		setDestDir(null)
	}

	const toggle = useCallback((entry: SelectedEntry) => {
		setSelected((cur) =>
			cur.some((s) => s.path === entry.path) ? cur.filter((s) => s.path !== entry.path) : [...cur, entry]
		)
	}, [])

	const canTransfer = selected.length > 0 && !!destDir

	const start = () =>
		action.run("Couldn't start the transfer", async () => {
			if (!destDir) return
			for (const item of selected) {
				// download: source is remote (item.path), destination dir is local (destDir).
				// upload:   source is local  (item.path), destination dir is remote (destDir).
				const remotePath = direction === "download" ? item.path : destDir
				const localPath = direction === "download" ? destDir : item.path
				await api.enqueueTransfer(client.id, direction, remotePath, localPath, item.isDir)
			}
			toast.success(
				`${selected.length} ${selected.length === 1 ? "transfer" : "transfers"} queued`,
				direction === "download" ? `Downloading from ${client.name}` : `Uploading to ${client.name}`
			)
			onQueued?.()
			onClose()
		})

	const remotePane = (
		<FileBrowser
			icon={<Server className="h-4 w-4 text-muted" />}
			load={loadRemote}
			onPathChange={direction === "upload" ? setDestDir : undefined}
			onToggle={toggle}
			selectable={direction === "download"}
			selected={selected}
			title={`${client.name} (guest)`}
		/>
	)
	const localPane = (
		<FileBrowser
			icon={<Monitor className="h-4 w-4 text-muted" />}
			load={loadLocal}
			onPathChange={direction === "download" ? setDestDir : undefined}
			onToggle={toggle}
			selectable={direction === "upload"}
			selected={selected}
			title="This computer"
		/>
	)

	// Left pane is always the source, right pane the destination.
	const left = direction === "download" ? remotePane : localPane
	const right = direction === "download" ? localPane : remotePane

	return (
		<Modal
			footer={
				<>
					<span className="mr-auto text-xs text-subtle">
						{selected.length > 0
							? `${selected.length} selected → ${destDir ?? "choose a destination folder"}`
							: "Select files or folders to transfer"}
					</span>
					<Button onClick={onClose} variant="outline">
						Cancel
					</Button>
					<Button
						disabled={!canTransfer}
						icon={
							direction === "download" ? <Download className="h-4 w-4" /> : <Upload className="h-4 w-4" />
						}
						loading={action.busy}
						onClick={start}
						variant="primary"
					>
						Transfer
					</Button>
				</>
			}
			onClose={onClose}
			open
			size="xl"
			title="Transfer files"
		>
			<div className="mb-4 flex items-center justify-center gap-1 rounded-xl bg-surface-2 p-1">
				{(["download", "upload"] as const).map((d) => (
					<button
						className={cn(
							"no-drag flex flex-1 items-center justify-center gap-2 rounded-lg px-3 py-1.5 text-sm font-medium transition",
							direction === d ? "bg-surface text-text shadow-sm" : "text-muted hover:text-text"
						)}
						key={d}
						onClick={() => setDir(d)}
						type="button"
					>
						{d === "download" ? <Download className="h-4 w-4" /> : <Upload className="h-4 w-4" />}
						{d === "download" ? "Download from guest" : "Upload to guest"}
					</button>
				))}
			</div>

			<div className="flex items-stretch gap-3">
				<div className="flex min-w-0 flex-1 flex-col">
					<div className="mb-1.5 text-[0.7rem] font-semibold uppercase tracking-wide text-subtle">Source</div>
					{left}
				</div>
				<div className="grid shrink-0 place-items-center pt-8 text-subtle">
					<ArrowRight className="h-5 w-5" />
				</div>
				<div className="flex min-w-0 flex-1 flex-col">
					<div className="mb-1.5 text-[0.7rem] font-semibold uppercase tracking-wide text-subtle">
						Destination folder
					</div>
					{right}
				</div>
			</div>
		</Modal>
	)
}
