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

/// Where the client should connect, and how it enrols. `controller_key` is the
/// controller's Ed25519 public key, pinned into the agent so it only accepts
/// control from this controller.
pub enum DeployTarget {
	/// Dial the controller directly (optionally joining Tailscale first).
	Controller {
		address: String,
		auth_key: Option<String>,
		controller_key: String,
	},
	/// Dial the relay (`libretether-relay`) with an agent secret.
	Relay {
		address: String,
		agent_secret: String,
		controller_key: String,
	},
}

/// Render the deploy command for a client: a one-liner that runs the published
/// installer for `os` with this client's enrollment arguments. The result is
/// bare (no shebang or comments) so it can be pasted straight into a shell; the
/// UI adds a shebang only when saving it to a file.
pub fn script(os: ClientOs, token: &str, target: &DeployTarget) -> String {
	match os {
		ClientOs::Windows => {
			let url = format!("{RELEASE_BASE}/install-windows.ps1");
			format!("& ([scriptblock]::Create((irm {url}))) {}", win_args(token, target))
		}
		ClientOs::Linux | ClientOs::Macos => {
			let installer = match os {
				ClientOs::Macos => "install-macos.sh",
				_ => "install-linux.sh",
			};
			let url = format!("{RELEASE_BASE}/{installer}");
			format!("curl -fsSL {url} | sh -s -- {}", sh_args(token, target))
		}
	}
}

/// POSIX-shell installer arguments. Every value is single-quoted with embedded
/// single quotes escaped, so a stray quote in an operator-entered address/key
/// can't break out of the quoting into the generated command.
fn sh_args(token: &str, target: &DeployTarget) -> String {
	match target {
		DeployTarget::Relay {
			address,
			agent_secret,
			controller_key,
		} => format!(
			"--token {} --relay {} --relay-secret {} --controller-key {}",
			sh_quote(token),
			sh_quote(address),
			sh_quote(agent_secret),
			sh_quote(controller_key),
		),
		DeployTarget::Controller {
			address,
			auth_key: Some(key),
			controller_key,
		} => format!(
			"--token {} --controller {} --tailscale-key {} --controller-key {}",
			sh_quote(token),
			sh_quote(address),
			sh_quote(key),
			sh_quote(controller_key),
		),
		DeployTarget::Controller {
			address,
			auth_key: None,
			controller_key,
		} => format!(
			"--token {} --controller {} --controller-key {}",
			sh_quote(token),
			sh_quote(address),
			sh_quote(controller_key),
		),
	}
}

/// PowerShell installer arguments (single-quoted literal strings, embedded
/// quotes escaped by doubling).
fn win_args(token: &str, target: &DeployTarget) -> String {
	match target {
		DeployTarget::Relay {
			address,
			agent_secret,
			controller_key,
		} => format!(
			"-Token {} -Relay {} -RelaySecret {} -ControllerKey {}",
			ps_quote(token),
			ps_quote(address),
			ps_quote(agent_secret),
			ps_quote(controller_key),
		),
		DeployTarget::Controller {
			address,
			auth_key: Some(key),
			controller_key,
		} => format!(
			"-Token {} -Controller {} -TailscaleKey {} -ControllerKey {}",
			ps_quote(token),
			ps_quote(address),
			ps_quote(key),
			ps_quote(controller_key),
		),
		DeployTarget::Controller {
			address,
			auth_key: None,
			controller_key,
		} => format!(
			"-Token {} -Controller {} -ControllerKey {}",
			ps_quote(token),
			ps_quote(address),
			ps_quote(controller_key),
		),
	}
}

/// Single-quote a value for POSIX `sh`, escaping embedded single quotes.
fn sh_quote(s: &str) -> String {
	format!("'{}'", s.replace('\'', "'\\''"))
}

/// Single-quote a value for PowerShell, escaping embedded single quotes.
fn ps_quote(s: &str) -> String {
	format!("'{}'", s.replace('\'', "''"))
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
			controller_key: "ckey".into(),
		};
		let linux = script(ClientOs::Linux, "tok", &relay);
		assert!(linux.contains("releases/latest/download/install-linux.sh"), "{linux}");
		assert!(
			linux.contains(
				"| sh -s -- --token 'tok' --relay 'relay.example:47600' --relay-secret 'sekret' --controller-key 'ckey'"
			),
			"{linux}"
		);
		// Bare command — no shebang or comment lines.
		assert!(!linux.contains('#'), "{linux}");

		let macos = script(ClientOs::Macos, "tok", &relay);
		assert!(macos.contains("releases/latest/download/install-macos.sh"), "{macos}");

		let direct = DeployTarget::Controller {
			address: "ctl:47600".into(),
			auth_key: Some("tskey-abc".into()),
			controller_key: "ckey".into(),
		};
		let win = script(ClientOs::Windows, "tok", &direct);
		assert!(win.contains("releases/latest/download/install-windows.ps1"), "{win}");
		assert!(
			win.contains("-Token 'tok' -Controller 'ctl:47600' -TailscaleKey 'tskey-abc' -ControllerKey 'ckey'"),
			"{win}"
		);
	}

	// A single quote in an operator-entered address must be escaped, not allowed
	// to break out of the quoting in the generated one-liner.
	#[test]
	fn deploy_escapes_embedded_quotes() {
		let relay = DeployTarget::Relay {
			address: "evil';rm -rf ~;'".into(),
			agent_secret: "sekret".into(),
			controller_key: "ckey".into(),
		};
		let linux = script(ClientOs::Linux, "tok", &relay);
		assert!(linux.contains(r"'evil'\'';rm -rf ~;'\'''"), "{linux}");

		let win = script(ClientOs::Windows, "tok", &relay);
		assert!(win.contains("'evil'';rm -rf ~;'''"), "{win}");
	}
}
