//! LibreTether agent — a small headless daemon that keeps a machine reachable for
//! the LibreTether controller. It dials the controller over the tailnet, proves its
//! identity, and then serves status, command-exec, screenshot and live
//! screen-control requests.

// On Windows, build the (release) binary into the GUI subsystem so the always-on
// service runs with no console window — a console window would otherwise pop up
// at logon and closing it would kill the agent. CLI output still appears when run
// from a terminal because the child inherits the parent's console handles; debug
// builds keep a normal console for development.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod capture;
mod config;
mod handlers;
mod host;
mod input;
mod net;
#[cfg(target_os = "linux")]
mod platform;
mod proc;
#[cfg(target_os = "linux")]
mod pwstream;
mod rdp;
mod service;
mod session;
mod ssh_server;
#[cfg(target_os = "linux")]
mod wayland;
mod x11env;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{default_config_path, normalize_addr, AgentConfig};
use libretether_protocol::crypto::Identity;

#[derive(Parser)]
#[command(name = "libretether-agent", version, about = "LibreTether remote-control agent")]
struct Cli {
	/// Path to the agent config file.
	#[arg(long, global = true)]
	config: Option<PathBuf>,

	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand)]
enum Command {
	/// Write config + a fresh identity for a controller (or relay) and token.
	Enroll {
		/// Controller address as `host[:port]` (direct/Tailscale mode).
		#[arg(long)]
		controller: Option<String>,
		/// Relay address as `host[:port]` (relay mode).
		#[arg(long)]
		relay: Option<String>,
		/// Agent secret for the relay (relay mode).
		#[arg(long)]
		relay_secret: Option<String>,
		/// One-time enrollment token from the controller.
		#[arg(long)]
		token: String,
		/// Base64 Ed25519 public key of the controller, pinned so the agent only
		/// accepts control from that controller. Required (supplied by the deploy
		/// command) — there is no trust-on-first-use.
		#[arg(long)]
		controller_key: String,
		/// TLS server name to expect (advanced; defaults to libretether.local).
		#[arg(long, default_value = "libretether.local")]
		server_name: String,
	},
	/// Run the agent in the foreground (what the service executes).
	Run,
	/// Print local status and where the agent is configured to connect.
	Status,
	/// Install the always-on background service for the current user.
	Install,
	/// Remove the background service.
	Uninstall,
}

fn main() -> Result<()> {
	let cli = Cli::parse();
	let cfg_path = cli.config.clone().unwrap_or_else(default_config_path);

	// For the long-lived `run` service, recover the X11 session's DISPLAY/XAUTHORITY
	// **now**, while the process is still single-threaded — mutating the process
	// environment (which `x11env::ensure` does, and which xcap/enigo later read) is
	// only sound before the async runtime spawns worker threads. In the common case
	// (a graphical session already exists) this satisfies it once here, so the
	// per-session `ensure()` calls on runtime threads become no-ops; the per-session
	// retry remains only for the boot-before-login window, where no session yet
	// exists for this early call to find.
	#[cfg(target_os = "linux")]
	if matches!(cli.command, Command::Run) {
		x11env::ensure();
	}

	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()?
		.block_on(dispatch(cli, cfg_path))
}

async fn dispatch(cli: Cli, cfg_path: std::path::PathBuf) -> Result<()> {
	match cli.command {
		Command::Enroll {
			controller,
			relay,
			relay_secret,
			token,
			controller_key,
			server_name,
		} => {
			let (controller_addr, relay_addr) = match (controller, relay) {
				(Some(c), None) => (normalize_addr(&c), None),
				(None, Some(r)) => (String::new(), Some(normalize_addr(&r))),
				(Some(_), Some(_)) => anyhow::bail!("provide only one of --controller or --relay"),
				(None, None) => anyhow::bail!("provide either --controller or --relay"),
			};
			if controller_key.trim().is_empty() {
				anyhow::bail!("--controller-key must not be empty");
			}
			let identity = Identity::generate();
			let cfg = AgentConfig {
				controller_addr,
				relay_addr,
				relay_secret,
				server_name,
				enrollment_token: Some(token),
				controller_key: Some(controller_key),
				identity_seed: identity.seed_b64(),
				client_id: None,
			};
			cfg.save(&cfg_path)?;
			println!("Enrolled. Config written to {}", cfg_path.display());
			println!("Public key: {}", identity.public_b64());
			println!("Next: `libretether-agent install` to run it in the background, or `libretether-agent run`.");
			Ok(())
		}
		Command::Run => net::run(cfg_path).await,
		Command::Status => {
			let info = host::host_info();
			println!("host:      {} ({}, {})", info.hostname, info.os, info.arch);
			println!("user:      {}", info.username);
			#[cfg(target_os = "linux")]
			println!(
				"session:   {}",
				if platform::is_wayland() {
					"wayland (portals + pipewire)"
				} else {
					"x11"
				}
			);
			println!("displays:  {}", capture::display_count());
			match AgentConfig::load(&cfg_path) {
				Ok(cfg) => {
					println!("config:    {}", cfg_path.display());
					match cfg.relay() {
						Some(relay) => println!("relay:     {relay}"),
						None => println!("controller:{}", cfg.controller_addr),
					}
					println!(
						"enrolled:  {}",
						if cfg.enrollment_token.is_none() {
							"yes"
						} else {
							"no (token pending)"
						}
					);
					if let Some(id) = cfg.client_id {
						println!("client id: {id}");
					}
				}
				Err(_) => println!("config:    not found at {} (run `enroll` first)", cfg_path.display()),
			}
			Ok(())
		}
		Command::Install => {
			AgentConfig::load(&cfg_path).context("no config found — run `libretether-agent enroll` first")?;
			service::install(&cfg_path)
		}
		Command::Uninstall => service::uninstall(),
	}
}
