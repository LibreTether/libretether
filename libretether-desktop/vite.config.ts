import { fileURLToPath, URL } from "node:url"
import tailwindcss from "@tailwindcss/vite"
import react from "@vitejs/plugin-react"
import { defineConfig } from "vite"
import { version } from "./package.json"

const host = process.env.TAURI_DEV_HOST

// https://vite.dev/config/
export default defineConfig(async () => ({
	// Target the (modern) WebKitGTK / WKWebView runtimes that Tauri ships against.
	build: {
		minify: !process.env.TAURI_ENV_DEBUG ? "esbuild" : false,
		sourcemap: !!process.env.TAURI_ENV_DEBUG,
		target: process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome110" : "safari15"
	},

	// Tauri expects a fixed port, fail if that port is not available
	clearScreen: false,
	// Surface the package version to the app (e.g. the controller-select footer)
	// without a runtime IPC round-trip — it's baked in at build time.
	define: {
		__APP_VERSION__: JSON.stringify(version)
	},
	plugins: [react(), tailwindcss()],

	resolve: {
		alias: {
			"@": fileURLToPath(new URL("./src", import.meta.url))
		}
	},
	server: {
		hmr: host
			? {
					host,
					port: 1421,
					protocol: "ws"
				}
			: undefined,
		host: host || false,
		port: 1420,
		strictPort: true,
		watch: {
			// Tell Vite to ignore watching `src-tauri`
			ignored: ["**/src-tauri/**"]
		}
	}
}))
