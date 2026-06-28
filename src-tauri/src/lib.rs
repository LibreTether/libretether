mod commands;
mod deploy;
mod error;
mod rdp;
mod registry;
mod server;
mod session;
mod state;
mod tailscale;

use std::path::PathBuf;

use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
	// WebKitGTK's DMABUF renderer causes blank/garbled webviews on a number of
	// Linux GPU/driver setups (notably NVIDIA proprietary). Disable it before
	// the webview spins up, honouring any value the user already exported.
	#[cfg(target_os = "linux")]
	if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
		std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
	}

	let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("tether");
	let state = AppState::init(config_dir).expect("failed to initialise controller state");

	tauri::Builder::default()
		.plugin(tauri_plugin_os::init())
		.plugin(tauri_plugin_dialog::init())
		.plugin(tauri_plugin_opener::init())
		.plugin(tauri_plugin_notification::init())
		.plugin(tauri_plugin_clipboard_manager::init())
		.manage(state.clone())
		.setup(move |app| {
			// Hand the state an app handle so it can emit change events, then
			// start the QUIC listener that agents dial into.
			state.set_app(app.handle().clone());
			let serve_state = state.clone();
			tauri::async_runtime::spawn(async move {
				server::serve(serve_state).await;
			});
			Ok(())
		})
		.invoke_handler(tauri::generate_handler![
			commands::list_clients,
			commands::create_client,
			commands::remove_client,
			commands::rename_client,
			commands::get_deploy_script,
			commands::reset_token,
			commands::client_status,
			commands::client_exec,
			commands::client_screenshot,
			commands::start_control,
			commands::send_input,
			commands::stop_control,
			commands::connect_rdp,
			commands::save_text_file,
			commands::controller_info,
			commands::set_controller_settings,
		])
		.run(tauri::generate_context!())
		.expect("error while running Tether");
}
