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
mod encode;
mod handlers;
mod host;
mod input;
#[cfg(all(target_os = "windows", feature = "media-foundation"))]
mod mf_encoder;
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
#[cfg(target_os = "windows")]
mod wincap;
mod x11env;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::{default_config_path, normalize_addr, AgentConfig};
use libretether_protocol::crypto::Identity;
use libretether_protocol::pairing::{PairBundle, PairingCode};

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
	/// Pair with a controller through a relay using a short spoken code, then write
	/// config — the phone-friendly alternative to `enroll` with no keys to type. The
	/// controller hands over the enrollment details over a PAKE-secured channel.
	Pair {
		/// Relay address as `host[:port]` (the same relay the controller uses).
		#[arg(long)]
		relay: String,
		/// The pairing code from the controller, `NAMEPLATE-PASSWORD` (e.g. 4F9K-2A7C).
		#[arg(long)]
		code: String,
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

	let result = dispatch_blocking(cli, cfg_path.clone());
	// Leave a trace even for the one-shot commands (enroll/pair/install). On the
	// Windows GUI-subsystem build there's no console and stderr is discarded, so a
	// failure would otherwise vanish with no `agent.log` (only `run` writes one) —
	// append it next to the config so the installer can point an operator at it.
	if let Err(err) = &result {
		log_fatal(&cfg_path, err);
	}
	result
}

fn dispatch_blocking(cli: Cli, cfg_path: PathBuf) -> Result<()> {
	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()?
		.block_on(dispatch(cli, cfg_path))
}

/// Append a fatal error to `agent.log` beside the config (best-effort), so a failed
/// one-shot command leaves a diagnosable trace where stderr goes nowhere.
fn log_fatal(cfg_path: &std::path::Path, err: &anyhow::Error) {
	let Some(dir) = cfg_path.parent() else { return };
	if std::fs::create_dir_all(dir).is_err() {
		return;
	}
	if let Ok(mut f) = std::fs::OpenOptions::new()
		.create(true)
		.append(true)
		.open(dir.join("agent.log"))
	{
		use std::io::Write;
		let _ = writeln!(f, "[{}] error: {err:#}", host::now_secs());
	}
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
		Command::Pair {
			relay,
			code,
			server_name,
		} => {
			let code = PairingCode::parse(&code).ok_or_else(|| {
				anyhow::anyhow!("invalid pairing code — expected the NAMEPLATE-PASSWORD the controller showed")
			})?;
			let relay = normalize_addr(&relay);
			println!("Pairing with the controller via {relay}…");
			let (bundle, phrase) = net::pair(&relay, &code, &server_name).await?;
			// The PAKE already guarantees no one is in the middle; the phrase lets the
			// human confirm the *right* machine paired (it matches the controller's).
			println!("Verify phrase: {phrase}");
			let identity = Identity::generate();
			let cfg = config_from_pairing(relay, server_name, bundle, &identity);
			cfg.save(&cfg_path)?;
			println!("Paired. Config written to {}", cfg_path.display());
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

/// Build the agent config from a pairing bundle. Pairing is always relay mode (the
/// machine reached the controller *through* the relay it just paired over), so the
/// relay address and agent secret come from there; the controller key is pinned
/// exactly as a pasted `enroll --controller-key` would, with no trust-on-first-use.
fn config_from_pairing(relay: String, server_name: String, bundle: PairBundle, identity: &Identity) -> AgentConfig {
	AgentConfig {
		controller_addr: String::new(),
		relay_addr: Some(relay),
		relay_secret: Some(bundle.agent_secret),
		server_name,
		enrollment_token: Some(bundle.enrollment_token),
		controller_key: Some(bundle.controller_key),
		identity_seed: identity.seed_b64(),
		client_id: None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn config_from_pairing_pins_the_bundle_in_relay_mode() {
		let bundle = PairBundle {
			enrollment_token: "tok-1".into(),
			controller_key: "Q3RybEtleQ==".into(),
			agent_secret: "agent-sekret".into(),
			name: Some("kitchen-pc".into()),
		};
		let id = Identity::generate();
		let cfg = config_from_pairing("relay.example:47600".into(), "libretether.local".into(), bundle, &id);

		// Relay mode with the bundle's routing + pinned controller key.
		assert_eq!(cfg.relay(), Some("relay.example:47600"));
		assert_eq!(cfg.relay_secret.as_deref(), Some("agent-sekret"));
		assert_eq!(cfg.enrollment_token.as_deref(), Some("tok-1"));
		assert_eq!(cfg.require_controller_key().unwrap(), "Q3RybEtleQ==");
		assert!(cfg.controller_addr.is_empty(), "pairing never uses direct mode");
		// The identity is usable (round-trips to the same public key).
		assert_eq!(cfg.identity().unwrap().public_b64(), id.public_b64());
	}
}
