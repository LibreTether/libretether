//! Best-effort discovery of the live X11 session for a background service.

/// Populate `DISPLAY`/`XAUTHORITY` from the running graphical session when
/// they're missing or unauthenticated, so the X11 capture (`xcap`) and input
/// (`enigo`) backends can connect to the X server.
///
/// A `systemd --user` service starts at boot and inherits neither variable, so
/// every X11 call would otherwise fail with "Authorization required, but no
/// authorization protocol specified". We recover them by borrowing a *consistent*
/// pair from a process already in the user's session — read via
/// `/proc/<pid>/environ`, which the kernel only exposes for our own user's
/// processes — and fall back to the X socket for the display number.
///
/// Idempotent and cheap once satisfied; a no-op off Linux.
///
/// `main` calls this **once at startup, before building the async runtime** — i.e.
/// while the process is still single-threaded — so in the common case (a graphical
/// session already exists) the one `set_var` happens with no other threads running,
/// which is the only point at which mutating the process environment is sound. The
/// later per-session calls (before the capture/injector threads spin up) then find
/// `applied` already true and do nothing. The residual single-threadedness caveat
/// applies only to the boot-before-login window, where the startup call finds no
/// session yet and a later call has to set the env once a session appears.
#[cfg(target_os = "linux")]
pub fn ensure() {
	use std::path::Path;
	use std::sync::Mutex;

	// Serialize so two concurrent callers (e.g. a screenshot during a live session)
	// never run `set_var` at the same time, and so we stop touching the process
	// environment once it's been satisfied — `getenv`/`setenv` are not thread-safe,
	// and the X11 backends read these vars on their own threads. `applied` flips to
	// true only once the env is fully usable, so an early call before the graphical
	// session exists still retries later (see the startup call in `main`).
	static APPLIED: Mutex<bool> = Mutex::new(false);
	let mut applied = APPLIED.lock().unwrap();
	if *applied {
		return;
	}

	let xauth_ok = std::env::var_os("XAUTHORITY").is_some_and(|p| Path::new(&p).exists());
	if std::env::var_os("DISPLAY").is_some() && xauth_ok {
		*applied = true;
		return;
	}

	if let Some((display, xauthority)) = borrow_from_session() {
		// A matched pair from one process — overrides any stale DISPLAY hint
		// (e.g. a hardcoded :0) so it can't disagree with the cookie.
		std::env::set_var("DISPLAY", display);
		std::env::set_var("XAUTHORITY", xauthority);
		*applied = true;
		return;
	}

	// No borrowable session env (e.g. a classic `startx` with ~/.Xauthority):
	// at least point DISPLAY at a live X socket and let Xlib find the cookie. Leave
	// `applied` false so a later call can still upgrade to a borrowed pair.
	if std::env::var_os("DISPLAY").is_none() {
		if let Some(display) = display_from_socket() {
			std::env::set_var("DISPLAY", display);
		}
	}
}

#[cfg(not(target_os = "linux"))]
pub fn ensure() {}

/// Find a same-user process already talking to X and adopt its `DISPLAY` +
/// `XAUTHORITY` (a guaranteed-consistent pair). Other users' `environ` is
/// unreadable, so we naturally only inspect our own processes; the agent's own
/// process lacks a valid `XAUTHORITY` and so is skipped.
#[cfg(target_os = "linux")]
fn borrow_from_session() -> Option<(String, String)> {
	for entry in std::fs::read_dir("/proc").ok()?.flatten() {
		let name = entry.file_name();
		let is_pid = name
			.to_str()
			.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
		if !is_pid {
			continue;
		}
		let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
			continue;
		};
		let mut display = None;
		let mut xauthority = None;
		for var in environ.split(|&b| b == 0) {
			if let Some(v) = var.strip_prefix(b"DISPLAY=") {
				display = std::str::from_utf8(v).ok().filter(|s| !s.is_empty());
			} else if let Some(v) = var.strip_prefix(b"XAUTHORITY=") {
				xauthority = std::str::from_utf8(v).ok().filter(|s| !s.is_empty());
			}
		}
		if let (Some(d), Some(x)) = (display, xauthority) {
			if std::path::Path::new(x).exists() {
				return Some((d.to_owned(), x.to_owned()));
			}
		}
	}
	None
}

/// The lowest-numbered display with a live socket in `/tmp/.X11-unix` (`X0` → `:0`).
#[cfg(target_os = "linux")]
fn display_from_socket() -> Option<String> {
	let mut lowest: Option<u32> = None;
	for entry in std::fs::read_dir("/tmp/.X11-unix").ok()?.flatten() {
		let name = entry.file_name();
		if let Some(num) = name
			.to_str()
			.and_then(|s| s.strip_prefix('X'))
			.and_then(|n| n.parse::<u32>().ok())
		{
			lowest = Some(lowest.map_or(num, |b| b.min(num)));
		}
	}
	lowest.map(|n| format!(":{n}"))
}
