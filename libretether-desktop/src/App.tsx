import { useCallback, useEffect, useState } from "react"
import { ControlOverlay } from "./components/ControlOverlay"
import { ConfirmProvider } from "./components/confirm"
import { type Page, Sidebar } from "./components/Sidebar"
import { Spinner } from "./components/ui"
import * as api from "./lib/api"
import { ToastProvider, useToast } from "./lib/toast"
import type { ActiveInfo, ClientDto } from "./lib/types"
import { ConnectionPage } from "./pages/ConnectionPage"
import { ControllerSelect } from "./pages/ControllerSelect"
import { LogsPage } from "./pages/LogsPage"
import { MachinesPage } from "./pages/MachinesPage"

function Shell({ active, onExit }: { active: ActiveInfo; onExit: () => void }) {
	const toast = useToast()
	const [page, setPage] = useState<Page>("machines")
	const [clients, setClients] = useState<ClientDto[]>([])
	const [loading, setLoading] = useState(true)
	const [control, setControl] = useState<ClientDto | null>(null)

	const reload = useCallback(() => {
		api.listClients()
			.then(setClients)
			.catch((e) => toast.error("Couldn't load machines", api.errString(e)))
			.finally(() => setLoading(false))
	}, [toast])

	useEffect(() => {
		reload()
		const unlisten = api.onClientsChanged(reload)
		return () => {
			unlisten.then((fn) => fn())
		}
	}, [reload])

	const exit = async () => {
		try {
			await api.exitController()
		} catch {
			/* exiting regardless */
		}
		onExit()
	}

	const onlineCount = clients.filter((c) => c.online).length

	return (
		<div className="flex h-screen overflow-hidden">
			<Sidebar active={active} onExit={exit} onlineCount={onlineCount} onNavigate={setPage} page={page} />
			<main className="flex min-w-0 flex-1 flex-col overflow-hidden">
				{page === "machines" && (
					<MachinesPage clients={clients} loading={loading} onControl={setControl} onReload={reload} />
				)}
				{page === "controller" && <ConnectionPage active={active} />}
				{page === "logs" && <LogsPage clients={clients} />}
			</main>

			{control && <ControlOverlay client={control} onClose={() => setControl(null)} />}
		</div>
	)
}

function Root() {
	const [active, setActive] = useState<ActiveInfo | null>(null)
	const [loading, setLoading] = useState(true)

	useEffect(() => {
		api.activeController()
			.then(setActive)
			.catch(() => setActive(null))
			.finally(() => setLoading(false))
	}, [])

	if (loading) {
		return (
			<div className="grid h-screen place-items-center">
				<Spinner className="h-6 w-6" />
			</div>
		)
	}

	return active ? (
		<Shell active={active} onExit={() => setActive(null)} />
	) : (
		<ControllerSelect onConnected={setActive} />
	)
}

export default function App() {
	return (
		<ToastProvider>
			<ConfirmProvider>
				<Root />
			</ConfirmProvider>
		</ToastProvider>
	)
}
