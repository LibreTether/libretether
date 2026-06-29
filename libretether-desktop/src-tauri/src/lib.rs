mod commands;
mod deploy;
mod error;
mod launch;
mod link;
mod rdp;
mod registry;
mod server;
mod session;
mod ssh;
mod state;
mod tailscale;
mod tunnel;
mod window;

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

	let config_dir = dirs::config_dir()
		.unwrap_or_else(|| PathBuf::from("."))
		.join("libretether");
	let state = AppState::init(config_dir).expect("failed to initialise controller state");

	tauri::Builder::default()
		.plugin(tauri_plugin_os::init())
		.plugin(tauri_plugin_dialog::init())
		.plugin(tauri_plugin_opener::init())
		.plugin(tauri_plugin_notification::init())
		.plugin(tauri_plugin_clipboard_manager::init())
		.manage(state.clone())
		.setup(move |app| {
			// Hand the state an app handle so it can emit change events. No
			// controller serves until the user selects one on the launch screen.
			state.set_app(app.handle().clone());
			// Put the native window controls where the user's desktop wants them.
			window::honor_button_layout(app.handle());
			Ok(())
		})
		.invoke_handler(tauri::generate_handler![
			commands::list_controllers,
			commands::create_controller,
			commands::update_controller,
			commands::delete_controller,
			commands::select_controller,
			commands::exit_controller,
			commands::active_controller,
			commands::get_settings,
			commands::set_settings,
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
			commands::connect_ssh,
			commands::save_text_file,
		])
		.run(tauri::generate_context!())
		.expect("error while running LibreTether");
}
