//! Generates the one-click deployment command for a client.
//!
//! The actual install logic lives in the published, versioned installers
//! (`scripts/install/install-{linux,macos,windows}.{sh,ps1}`, uploaded as release
//! assets). This only renders a one-liner that fetches the right installer and
//! passes the client's enrollment token and connection details as arguments —
//! so the install steps (Tailscale, runtime libs, agent download, enroll, and
//! service install) live in exactly one place.
//!
//! The installer honours `LIBRETETHER_AGENT_BIN` / `LIBRETETHER_AGENT_URL`, so a
//! user can prefix the generated command with those to use a local/custom build.

use crate::registry::ClientOs;

/// Base URL for the published release assets (installers + agent binaries).
const RELEASE_BASE: &str = "https://github.com/LibreTether/libretether/releases/latest/download";

/// Where the client should connect, and how it enrols.
pub enum DeployTarget {
	/// Dial the controller directly (optionally joining Tailscale first).
	Controller { address: String, auth_key: Option<String> },
	/// Dial the relay (`libretether-relay`) with an agent secret.
	Relay { address: String, agent_secret: String },
}

/// Render the deploy command for a client: a one-liner that runs the published
/// installer for `os` with this client's enrollment arguments.
pub fn script(name: &str, os: ClientOs, token: &str, target: &DeployTarget) -> String {
	match os {
		ClientOs::Windows => {
			let url = format!("{RELEASE_BASE}/install-windows.ps1");
			let args = win_args(token, target);
			format!(
				"# LibreTether agent deployment — {name} (windows)\n\
				 # Paste into a PowerShell prompt on the client machine.\n\
				 & ([scriptblock]::Create((irm {url}))) {args}\n"
			)
		}
		ClientOs::Linux | ClientOs::Macos => {
			let installer = match os {
				ClientOs::Macos => "install-macos.sh",
				_ => "install-linux.sh",
			};
			let url = format!("{RELEASE_BASE}/{installer}");
			let args = sh_args(token, target);
			format!(
				"#!/usr/bin/env sh\n\
				 # LibreTether agent deployment — {name}\n\
				 # Run this on the client machine you want to control.\n\
				 curl -fsSL {url} | sh -s -- {args}\n"
			)
		}
	}
}

/// POSIX-shell installer arguments. Values are single-quoted; tokens, secrets,
/// addresses and Tailscale keys are alphanumeric / `host:port`, so none can
/// contain a single quote that would break the quoting.
fn sh_args(token: &str, target: &DeployTarget) -> String {
	match target {
		DeployTarget::Relay { address, agent_secret } => {
			format!("--token '{token}' --relay '{address}' --relay-secret '{agent_secret}'")
		}
		DeployTarget::Controller {
			address,
			auth_key: Some(key),
		} => {
			format!("--token '{token}' --controller '{address}' --tailscale-key '{key}'")
		}
		DeployTarget::Controller {
			address,
			auth_key: None,
		} => {
			format!("--token '{token}' --controller '{address}'")
		}
	}
}

/// PowerShell installer arguments (single-quoted literal strings).
fn win_args(token: &str, target: &DeployTarget) -> String {
	match target {
		DeployTarget::Relay { address, agent_secret } => {
			format!("-Token '{token}' -Relay '{address}' -RelaySecret '{agent_secret}'")
		}
		DeployTarget::Controller {
			address,
			auth_key: Some(key),
		} => {
			format!("-Token '{token}' -Controller '{address}' -TailscaleKey '{key}'")
		}
		DeployTarget::Controller {
			address,
			auth_key: None,
		} => {
			format!("-Token '{token}' -Controller '{address}'")
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// The deploy command must invoke the published installer for the OS and pass
	// the agent's enroll flags — these arg names track the installers in
	// `scripts/install/` and the agent's `enroll` subcommand.
	#[test]
	fn deploy_invokes_the_published_installer() {
		let relay = DeployTarget::Relay {
			address: "relay.example:47600".into(),
			agent_secret: "sekret".into(),
		};
		let linux = script("box", ClientOs::Linux, "tok", &relay);
		assert!(linux.contains("releases/latest/download/install-linux.sh"), "{linux}");
		assert!(
			linux.contains("| sh -s -- --token 'tok' --relay 'relay.example:47600' --relay-secret 'sekret'"),
			"{linux}"
		);

		let macos = script("box", ClientOs::Macos, "tok", &relay);
		assert!(macos.contains("releases/latest/download/install-macos.sh"), "{macos}");

		let direct = DeployTarget::Controller {
			address: "ctl:47600".into(),
			auth_key: Some("tskey-abc".into()),
		};
		let win = script("box", ClientOs::Windows, "tok", &direct);
		assert!(win.contains("releases/latest/download/install-windows.ps1"), "{win}");
		assert!(
			win.contains("-Token 'tok' -Controller 'ctl:47600' -TailscaleKey 'tskey-abc'"),
			"{win}"
		);
	}
}
