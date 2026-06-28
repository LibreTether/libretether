//! Tether agent — a small headless daemon that keeps a machine reachable for
//! the Tether controller. It dials the controller over the tailnet, proves its
//! identity, and then serves status, command-exec, screenshot and live
//! screen-control requests.

mod capture;
mod config;
mod handlers;
mod host;
mod input;
mod net;
mod platform;
#[cfg(feature = "wayland-capture")]
mod pwstream;
mod service;
mod session;
#[cfg(feature = "wayland")]
mod wayland;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{default_config_path, normalize_addr, AgentConfig};
use tether_protocol::crypto::Identity;

#[derive(Parser)]
#[command(name = "tether-agent", version, about = "Tether remote-control agent")]
struct Cli {
	/// Path to the agent config file.
	#[arg(long, global = true)]
	config: Option<PathBuf>,

	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand)]
enum Command {
	/// Write config + a fresh identity for a controller and enrollment token.
	Enroll {
		/// Controller address as `host[:port]` (typically a tailnet name/IP).
		#[arg(long)]
		controller: String,
		/// One-time enrollment token from the controller.
		#[arg(long)]
		token: String,
		/// TLS server name to expect (advanced; defaults to tether.local).
		#[arg(long, default_value = "tether.local")]
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

#[tokio::main]
async fn main() -> Result<()> {
	let cli = Cli::parse();
	let cfg_path = cli.config.clone().unwrap_or_else(default_config_path);

	match cli.command {
		Command::Enroll {
			controller,
			token,
			server_name,
		} => {
			let identity = Identity::generate();
			let cfg = AgentConfig {
				controller_addr: normalize_addr(&controller),
				server_name,
				enrollment_token: Some(token),
				identity_seed: identity.seed_b64(),
				client_id: None,
			};
			cfg.save(&cfg_path)?;
			println!("Enrolled. Config written to {}", cfg_path.display());
			println!("Public key: {}", identity.public_b64());
			println!("Next: `tether-agent install` to run it in the background, or `tether-agent run`.");
			Ok(())
		}
		Command::Run => net::run(cfg_path).await,
		Command::Status => {
			let info = host::host_info();
			println!("host:      {} ({}, {})", info.hostname, info.os, info.arch);
			println!("user:      {}", info.username);
			println!("session:   {}", if platform::is_wayland() { "wayland" } else { "x11" });
			println!("displays:  {}", capture::display_count());
			println!(
				"wayland live capture: {}",
				if cfg!(feature = "wayland-capture") {
					"enabled (pipewire)"
				} else {
					"DISABLED — rebuild with `run build:agent`"
				}
			);
			match AgentConfig::load(&cfg_path) {
				Ok(cfg) => {
					println!("config:    {}", cfg_path.display());
					println!("controller:{}", cfg.controller_addr);
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
			AgentConfig::load(&cfg_path).context("no config found — run `tether-agent enroll` first")?;
			service::install(&cfg_path)
		}
		Command::Uninstall => service::uninstall(),
	}
}
