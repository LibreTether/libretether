//! Native window-chrome integration.
//!
//! On a Wayland session, tao gives the main window its *own* GTK `HeaderBar`
//! (see tao's `platform_impl/linux/wayland/header.rs`) and hardcodes the control
//! layout to `"menu:minimize,maximize,close"` — i.e. always on the right —
//! ignoring the user's GTK button-layout preference (e.g. left-hand controls).
//! We reach into that header bar and clear the hardcoded layout so the buttons
//! fall back to GTK's resolved `gtk-decoration-layout`, landing where the user
//! actually asked.
//!
//! On X11 tao installs no header bar of its own (the compositor/GTK draws the
//! CSD, already honouring the preference), so this is a no-op there.

/// Make the native window controls follow the user's GTK button-layout
/// preference. No-op on non-Linux platforms.
#[cfg(target_os = "linux")]
pub fn honor_button_layout(app: &tauri::AppHandle) {
	use gtk::prelude::*;
	use tauri::Manager;

	let Some(window) = app.get_webview_window("main") else {
		return;
	};
	let Ok(gtk_window) = window.gtk_window() else {
		return;
	};

	// tao wraps its header bar in an EventBox set as the window titlebar.
	// No titlebar (the X11 path) means there's nothing to adjust.
	let Some(titlebar) = gtk_window.titlebar() else {
		return;
	};
	let Some(header) = find_header_bar(&titlebar) else {
		return;
	};

	// Clearing the explicit layout makes the header bar inherit GTK's resolved
	// `gtk-decoration-layout`, which already reflects the desktop preference.
	header.set_decoration_layout(None);

	// tao re-applies its hardcoded layout whenever the window's resizability
	// changes; re-clear it afterwards so the user's preference keeps winning.
	let header = header.downgrade();
	gtk_window.connect_resizable_notify(move |_| {
		if let Some(header) = header.upgrade() {
			header.set_decoration_layout(None);
		}
	});
}

/// Depth-first search for the `HeaderBar` tao nests inside the titlebar widget.
#[cfg(target_os = "linux")]
fn find_header_bar(widget: &gtk::Widget) -> Option<gtk::HeaderBar> {
	use gtk::prelude::*;

	if let Some(header) = widget.downcast_ref::<gtk::HeaderBar>() {
		return Some(header.clone());
	}
	if let Some(container) = widget.downcast_ref::<gtk::Container>() {
		for child in container.children() {
			if let Some(header) = find_header_bar(&child) {
				return Some(header);
			}
		}
	}
	None
}

/// Make the native window controls follow the user's button-layout preference.
/// No-op on non-Linux platforms.
#[cfg(not(target_os = "linux"))]
pub fn honor_button_layout(_app: &tauri::AppHandle) {}
