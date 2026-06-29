//! Generates the one-click deployment script for a client. The script makes the
//! machine reachable by the controller, installs the agent binary, then enrolls
//! it and registers the always-on background service.
//!
//! Three connection modes, none of which requires the client to log in:
//! - **Tailscale auth key** — the script joins the tailnet non-interactively.
//! - **Direct** — the agent dials the controller's reachable address.
//! - **Relay** — the agent dials out to a `libretether-relay` relay; nothing on the
//!   client (or controller) needs to be exposed.
//!
//! By default the script downloads the agent for the client's OS/arch from the
//! project's latest GitHub release; `LIBRETETHER_AGENT_BIN` (a local path) or
//! `LIBRETETHER_AGENT_URL` (an explicit URL) override that for development.

use crate::registry::ClientOs;

/// Base URL for the published agent binaries — GitHub serves the newest release's
/// assets under `releases/latest/download/<asset>`. Change this in a fork.
const AGENT_RELEASE_BASE: &str = "https://github.com/LibreTether/libretether/releases/latest/download";

/// Where the client should connect, and how it enrols.
pub enum DeployTarget {
	/// Dial the controller directly (optionally joining Tailscale first).
	Controller { address: String, auth_key: Option<String> },
	/// Dial the relay (`libretether-relay`) with an agent secret.
	Relay { address: String, agent_secret: String },
}

impl DeployTarget {
	fn address(&self) -> &str {
		match self {
			DeployTarget::Controller { address, .. } | DeployTarget::Relay { address, .. } => address,
		}
	}
}

/// Render the deploy script for a client.
pub fn script(name: &str, os: ClientOs, token: &str, target: &DeployTarget) -> String {
	let template = match os {
		ClientOs::Linux => LINUX,
		ClientOs::Macos => MACOS,
		ClientOs::Windows => WINDOWS,
	};
	template
		.replace("__CONNECT_BLOCK__", &connect_block(os, target))
		.replace("__ENROLL__", &enroll_cmd(os, target))
		.replace("__AGENT_BASE__", AGENT_RELEASE_BASE)
		.replace("__NAME__", name)
		.replace("__CONTROLLER__", target.address())
		.replace("__TOKEN__", token)
}

/// The connectivity section, which differs by OS and mode.
fn connect_block(os: ClientOs, target: &DeployTarget) -> String {
	let win = matches!(os, ClientOs::Windows);
	match target {
		DeployTarget::Relay { address, .. } => format!(
			"# 1. This client dials out to the relay at {address} — nothing on this\n\
			 #    machine needs to be exposed or port-forwarded."
		),
		DeployTarget::Controller { address, auth_key: None } => format!(
			"# 1. Direct connection — this machine must be able to reach the controller at\n\
			 #    {address} (same LAN, an existing VPN, or a port-forward). No Tailscale needed."
		),
		DeployTarget::Controller { auth_key: Some(key), .. } if win => format!(
			"# 1. Join the controller's Tailscale network with a pre-auth key (no login).\n\
			 if (-not (Get-Command tailscale -ErrorAction SilentlyContinue)) {{\n\
			 \u{20}\u{20}Write-Host \"!! Install Tailscale from https://tailscale.com/download/windows, then re-run.\" -ForegroundColor Yellow; exit 1\n\
			 }}\n\
			 tailscale up --reset --authkey \"{key}\""
		),
		DeployTarget::Controller { auth_key: Some(key), .. } => format!(
			"# 1. Join the controller's Tailscale network with a pre-auth key (no login).\n\
			 if ! command -v tailscale >/dev/null 2>&1; then\n\
			 \u{20}\u{20}echo \"==> Installing Tailscale\"\n\
			 \u{20}\u{20}curl -fsSL https://tailscale.com/install.sh | sh\n\
			 fi\n\
			 sudo tailscale up --reset --authkey \"{key}\""
		),
	}
}

/// The enrollment command, which differs by OS shell and mode.
fn enroll_cmd(os: ClientOs, target: &DeployTarget) -> String {
	match (os, target) {
		(ClientOs::Windows, DeployTarget::Relay { agent_secret, .. }) => {
			format!("& $Bin enroll --relay $Controller --relay-secret \"{agent_secret}\" --token $Token")
		}
		(ClientOs::Windows, DeployTarget::Controller { .. }) => {
			"& $Bin enroll --controller $Controller --token $Token".to_string()
		}
		(_, DeployTarget::Relay { agent_secret, .. }) => {
			format!("\"$BIN\" enroll --relay \"$CONTROLLER\" --relay-secret \"{agent_secret}\" --token \"$TOKEN\"")
		}
		(_, DeployTarget::Controller { .. }) => {
			"\"$BIN\" enroll --controller \"$CONTROLLER\" --token \"$TOKEN\"".to_string()
		}
	}
}

const LINUX: &str = r#"#!/usr/bin/env bash
# LibreTether agent deployment — __NAME__ (linux)
# Run this on the CLIENT machine you want to control.
set -euo pipefail

CONTROLLER="__CONTROLLER__"
TOKEN="__TOKEN__"
BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/libretether-agent"

echo "==> LibreTether setup for __NAME__"

__CONNECT_BLOCK__

# 2. Install the runtime libraries the agent links against: libxdo (X11 input),
#    libxcb (X11 capture), and libpipewire (Wayland capture).
#    gnome-remote-desktop is for the optional RDP connect path on Wayland.
if command -v apt-get >/dev/null 2>&1; then
  sudo apt-get update -qq || true
  sudo apt-get install -y libxdo3 libxcb1 libxcb-randr0 libxcb-shm0 libxcb-xfixes0 libpipewire-0.3-0 gnome-remote-desktop || true
elif command -v dnf >/dev/null 2>&1; then
  sudo dnf install -y libxdo libxcb pipewire-libs gnome-remote-desktop || true
elif command -v pacman >/dev/null 2>&1; then
  sudo pacman -S --needed --noconfirm xdotool libxcb pipewire gnome-remote-desktop || true
else
  echo "!! Couldn't auto-install runtime libs — ensure libxdo (libxdo.so.3) and libxcb are present." >&2
fi

# 3. Install the agent binary. By default it is downloaded from the latest
#    LibreTether release for this machine's architecture; override with
#    LIBRETETHER_AGENT_BIN=/path/to/binary or LIBRETETHER_AGENT_URL=https://...
mkdir -p "$BIN_DIR"
if [ -n "${LIBRETETHER_AGENT_BIN:-}" ]; then
  install -m 0755 "$LIBRETETHER_AGENT_BIN" "$BIN"
else
  if [ -n "${LIBRETETHER_AGENT_URL:-}" ]; then
    URL="$LIBRETETHER_AGENT_URL"
  else
    case "$(uname -m)" in
      x86_64|amd64) ARCH=x86_64 ;;
      aarch64|arm64) ARCH=aarch64 ;;
      *) echo "!! Unsupported architecture $(uname -m). Set LIBRETETHER_AGENT_BIN or LIBRETETHER_AGENT_URL." >&2; exit 1 ;;
    esac
    URL="__AGENT_BASE__/libretether-agent-linux-$ARCH"
  fi
  echo "==> Downloading agent from $URL"
  curl -fsSL "$URL" -o "$BIN"
  chmod +x "$BIN"
fi

# 4. Enroll and install the always-on service.
__ENROLL__
"$BIN" install

echo "==> Done. __NAME__ is now reachable from your LibreTether controller."
"#;

const MACOS: &str = r#"#!/usr/bin/env bash
# LibreTether agent deployment — __NAME__ (macOS)
# Run this on the CLIENT Mac you want to control.
set -euo pipefail

CONTROLLER="__CONTROLLER__"
TOKEN="__TOKEN__"
BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/libretether-agent"

echo "==> LibreTether setup for __NAME__"

__CONNECT_BLOCK__

# 2. Install the agent binary (universal). By default it is downloaded from the
#    latest LibreTether release; override with LIBRETETHER_AGENT_BIN=/path or
#    LIBRETETHER_AGENT_URL=https://...
mkdir -p "$BIN_DIR"
if [ -n "${LIBRETETHER_AGENT_BIN:-}" ]; then
  install -m 0755 "$LIBRETETHER_AGENT_BIN" "$BIN"
else
  URL="${LIBRETETHER_AGENT_URL:-__AGENT_BASE__/libretether-agent-macos-universal}"
  echo "==> Downloading agent from $URL"
  curl -fsSL "$URL" -o "$BIN"
  chmod +x "$BIN"
fi

# 3. Enroll and install the LaunchAgent.
__ENROLL__
"$BIN" install

echo "==> Done. Grant Screen Recording + Accessibility to libretether-agent in System Settings > Privacy."
"#;

const WINDOWS: &str = r#"# LibreTether agent deployment — __NAME__ (windows)
# Run this on the CLIENT machine in an Administrator PowerShell. If you get a
# "running scripts is disabled" error, launch it with:
#   powershell -ExecutionPolicy Bypass -File .\this-script.ps1
$ErrorActionPreference = "Stop"

$Controller = "__CONTROLLER__"
$Token = "__TOKEN__"
$BinDir = Join-Path $env:LOCALAPPDATA "LibreTether"
$Bin = Join-Path $BinDir "libretether-agent.exe"

Write-Host "==> LibreTether setup for __NAME__"

__CONNECT_BLOCK__

# 2. Install the agent binary. By default it is downloaded from the latest
#    LibreTether release; override with $env:LIBRETETHER_AGENT_BIN or $env:LIBRETETHER_AGENT_URL.
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
if ($env:LIBRETETHER_AGENT_BIN) {
  Copy-Item $env:LIBRETETHER_AGENT_BIN $Bin -Force
} else {
  $Url = if ($env:LIBRETETHER_AGENT_URL) { $env:LIBRETETHER_AGENT_URL } else { "__AGENT_BASE__/libretether-agent-windows-x86_64.exe" }
  Write-Host "==> Downloading agent from $Url"
  Invoke-WebRequest -Uri $Url -OutFile $Bin
}

# 3. Enroll and register the logon task.
__ENROLL__
& $Bin install

Write-Host "==> Done. __NAME__ is now reachable from your LibreTether controller."
"#;

#[cfg(test)]
mod tests {
	use super::*;

	// The agent asset names must stay in lockstep with the release workflow
	// (`.github/workflows/release.yml` -> `libretether-agent-<key>[.exe]`).
	#[test]
	fn agent_is_fetched_from_the_latest_release() {
		let target = DeployTarget::Relay {
			address: "relay.example:47600".into(),
			agent_secret: "secret".into(),
		};
		let base = "releases/latest/download";

		let linux = script("box", ClientOs::Linux, "tok", &target);
		assert!(
			linux.contains(&format!("{base}/libretether-agent-linux-$ARCH")),
			"{linux}"
		);

		let macos = script("box", ClientOs::Macos, "tok", &target);
		assert!(
			macos.contains(&format!("{base}/libretether-agent-macos-universal")),
			"{macos}"
		);

		let windows = script("box", ClientOs::Windows, "tok", &target);
		assert!(
			windows.contains(&format!("{base}/libretether-agent-windows-x86_64.exe")),
			"{windows}"
		);

		// No template placeholder should survive rendering.
		for s in [&linux, &macos, &windows] {
			assert!(!s.contains("__"), "unsubstituted placeholder in:\n{s}");
		}
	}
}
