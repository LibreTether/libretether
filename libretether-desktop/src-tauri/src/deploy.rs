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

/// The ordered installer arguments for a deploy, each as `(sh_flag, ps_flag,
/// value)`. The two renderers below share this list so a new flag is added in one
/// place and can't drift between the POSIX and PowerShell commands.
fn installer_args<'a>(token: &'a str, target: &'a DeployTarget) -> Vec<(&'static str, &'static str, &'a str)> {
	let mut args = vec![("--token", "-Token", token)];
	match target {
		DeployTarget::Relay {
			address,
			agent_secret,
			controller_key,
		} => {
			args.push(("--relay", "-Relay", address));
			args.push(("--relay-secret", "-RelaySecret", agent_secret));
			args.push(("--controller-key", "-ControllerKey", controller_key));
		}
		DeployTarget::Controller {
			address,
			auth_key,
			controller_key,
		} => {
			args.push(("--controller", "-Controller", address));
			if let Some(key) = auth_key {
				args.push(("--tailscale-key", "-TailscaleKey", key));
			}
			args.push(("--controller-key", "-ControllerKey", controller_key));
		}
	}
	args
}

/// POSIX-shell installer arguments. Every value is single-quoted with embedded
/// single quotes escaped, so a stray quote in an operator-entered address/key
/// can't break out of the quoting into the generated command.
fn sh_args(token: &str, target: &DeployTarget) -> String {
	installer_args(token, target)
		.iter()
		.map(|(flag, _, value)| format!("{flag} {}", sh_quote(value)))
		.collect::<Vec<_>>()
		.join(" ")
}

/// PowerShell installer arguments (single-quoted literal strings, embedded
/// quotes escaped by doubling).
fn win_args(token: &str, target: &DeployTarget) -> String {
	installer_args(token, target)
		.iter()
		.map(|(_, flag, value)| format!("{flag} {}", ps_quote(value)))
		.collect::<Vec<_>>()
		.join(" ")
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

	// The Windows one-liner must be wrapped in the `irm | scriptblock` invocation so
	// it actually runs the fetched installer — the part most likely to break silently
	// if the wrapper is edited.
	#[test]
	fn windows_command_wraps_the_installer_in_a_scriptblock() {
		let direct = DeployTarget::Controller {
			address: "ctl:47600".into(),
			auth_key: None,
			controller_key: "ckey".into(),
		};
		let win = script(ClientOs::Windows, "tok", &direct);
		assert!(
			win.starts_with("& ([scriptblock]::Create((irm "),
			"the PowerShell wrapper must invoke the fetched installer: {win}"
		);
		// The closing parens of the scriptblock invocation precede the arguments.
		assert!(win.contains(".ps1))) -Token 'tok'"), "{win}");
	}
}
