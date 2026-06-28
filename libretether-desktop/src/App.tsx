import { useCallback, useEffect, useState } from "react"
import { ControlOverlay } from "./components/ControlOverlay"
import { ConfirmProvider } from "./components/confirm"
import { type Page, Sidebar } from "./components/Sidebar"
import * as api from "./lib/api"
import { ToastProvider, useToast } from "./lib/toast"
import type { ClientDto } from "./lib/types"
import { ControllerPage } from "./pages/ControllerPage"
import { MachinesPage } from "./pages/MachinesPage"

function Shell() {
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

	const onlineCount = clients.filter((c) => c.online).length

	return (
		<div className="flex h-screen overflow-hidden">
			<Sidebar onlineCount={onlineCount} onNavigate={setPage} page={page} />
			<main className="flex min-w-0 flex-1 flex-col overflow-hidden">
				{page === "machines" && (
					<MachinesPage clients={clients} loading={loading} onControl={setControl} onReload={reload} />
				)}
				{page === "controller" && <ControllerPage />}
			</main>

			{control && <ControlOverlay client={control} onClose={() => setControl(null)} />}
		</div>
	)
}

export default function App() {
	return (
		<ToastProvider>
			<ConfirmProvider>
				<Shell />
			</ConfirmProvider>
		</ToastProvider>
	)
}
