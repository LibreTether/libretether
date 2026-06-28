//! Generates the one-click deployment script for a client. The script makes the
//! machine reachable by the controller, installs the agent binary, then enrolls
//! it and registers the always-on background service.
//!
//! Connectivity has two modes, neither of which requires the client to log in:
//! - **Tailscale auth key** — when the controller has a pre-auth key configured,
//!   the script joins the tailnet non-interactively (`tailscale up --reset --authkey`).
//! - **Direct** — otherwise the agent just dials the controller's address, which
//!   must be reachable (same LAN, an existing VPN, or a port-forward).
//!
//! We do not host the agent binary ourselves (no cloud), so the script takes it
//! from `TETHER_AGENT_BIN` (a local path) or `TETHER_AGENT_URL` (a release asset).

use crate::registry::ClientOs;

/// Render the deploy script for a client. `auth_key` is the optional Tailscale
/// pre-auth key; when `None`, a direct connection is assumed.
pub fn script(name: &str, os: ClientOs, controller_addr: &str, token: &str, auth_key: Option<&str>) -> String {
	let template = match os {
		ClientOs::Linux => LINUX,
		ClientOs::Macos => MACOS,
		ClientOs::Windows => WINDOWS,
	};
	template
		.replace("__CONNECT_BLOCK__", &connect_block(os, controller_addr, auth_key))
		.replace("__NAME__", name)
		.replace("__CONTROLLER__", controller_addr)
		.replace("__TOKEN__", token)
}

/// The connectivity section, which differs by OS and mode.
fn connect_block(os: ClientOs, controller_addr: &str, auth_key: Option<&str>) -> String {
	match (os, auth_key) {
		(ClientOs::Windows, Some(key)) => format!(
			"# 1. Join the controller's Tailscale network with a pre-auth key (no login).\n\
			 if (-not (Get-Command tailscale -ErrorAction SilentlyContinue)) {{\n\
			 \u{20}\u{20}Write-Host \"!! Install Tailscale from https://tailscale.com/download/windows, then re-run.\" -ForegroundColor Yellow; exit 1\n\
			 }}\n\
			 tailscale up --reset --authkey \"{key}\""
		),
		(ClientOs::Windows, None) => format!(
			"# 1. Direct connection — this machine must be able to reach the controller at\n\
			 #    {controller_addr} (same LAN, an existing VPN, or a port-forward). No Tailscale needed."
		),
		(_, Some(key)) => format!(
			"# 1. Join the controller's Tailscale network with a pre-auth key (no login).\n\
			 if ! command -v tailscale >/dev/null 2>&1; then\n\
			 \u{20}\u{20}echo \"==> Installing Tailscale\"\n\
			 \u{20}\u{20}curl -fsSL https://tailscale.com/install.sh | sh\n\
			 fi\n\
			 sudo tailscale up --reset --authkey \"{key}\""
		),
		(_, None) => format!(
			"# 1. Direct connection — this machine must be able to reach the controller at\n\
			 #    {controller_addr} (same LAN, an existing VPN, or a port-forward). No Tailscale needed."
		),
	}
}

const LINUX: &str = r#"#!/usr/bin/env bash
# Tether agent deployment — __NAME__ (linux)
# Run this on the CLIENT machine you want to control.
set -euo pipefail

CONTROLLER="__CONTROLLER__"
TOKEN="__TOKEN__"
BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/tether-agent"

echo "==> Tether setup for __NAME__"

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

# 3. Install the agent binary.
#    Provide it via TETHER_AGENT_BIN=/path/to/tether-agent or TETHER_AGENT_URL=https://...
mkdir -p "$BIN_DIR"
if [ -n "${TETHER_AGENT_BIN:-}" ]; then
  install -m 0755 "$TETHER_AGENT_BIN" "$BIN"
elif [ -n "${TETHER_AGENT_URL:-}" ]; then
  echo "==> Downloading agent from $TETHER_AGENT_URL"
  curl -fsSL "$TETHER_AGENT_URL" -o "$BIN" && chmod +x "$BIN"
else
  echo "!! No agent binary source. Set TETHER_AGENT_BIN or TETHER_AGENT_URL and re-run." >&2
  exit 1
fi

# 4. Enroll with the controller and install the always-on service.
"$BIN" enroll --controller "$CONTROLLER" --token "$TOKEN"
"$BIN" install

echo "==> Done. __NAME__ is now reachable from your Tether controller."
"#;

const MACOS: &str = r#"#!/usr/bin/env bash
# Tether agent deployment — __NAME__ (macOS)
# Run this on the CLIENT Mac you want to control.
set -euo pipefail

CONTROLLER="__CONTROLLER__"
TOKEN="__TOKEN__"
BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/tether-agent"

echo "==> Tether setup for __NAME__"

__CONNECT_BLOCK__

# 2. Install the agent binary (set TETHER_AGENT_BIN or TETHER_AGENT_URL).
mkdir -p "$BIN_DIR"
if [ -n "${TETHER_AGENT_BIN:-}" ]; then
  install -m 0755 "$TETHER_AGENT_BIN" "$BIN"
elif [ -n "${TETHER_AGENT_URL:-}" ]; then
  echo "==> Downloading agent from $TETHER_AGENT_URL"
  curl -fsSL "$TETHER_AGENT_URL" -o "$BIN" && chmod +x "$BIN"
else
  echo "!! No agent binary source. Set TETHER_AGENT_BIN or TETHER_AGENT_URL and re-run." >&2
  exit 1
fi

# 3. Enroll and install the LaunchAgent.
"$BIN" enroll --controller "$CONTROLLER" --token "$TOKEN"
"$BIN" install

echo "==> Done. Grant Screen Recording + Accessibility to tether-agent in System Settings > Privacy."
"#;

const WINDOWS: &str = r#"# Tether agent deployment — __NAME__ (windows)
# Run this on the CLIENT machine in an Administrator PowerShell. If you get a
# "running scripts is disabled" error, launch it with:
#   powershell -ExecutionPolicy Bypass -File .\this-script.ps1
$ErrorActionPreference = "Stop"

$Controller = "__CONTROLLER__"
$Token = "__TOKEN__"
$BinDir = Join-Path $env:LOCALAPPDATA "Tether"
$Bin = Join-Path $BinDir "tether-agent.exe"

Write-Host "==> Tether setup for __NAME__"

__CONNECT_BLOCK__

# 2. Install the agent binary (set TETHER_AGENT_BIN or TETHER_AGENT_URL).
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
if ($env:TETHER_AGENT_BIN) {
  Copy-Item $env:TETHER_AGENT_BIN $Bin -Force
} elseif ($env:TETHER_AGENT_URL) {
  Write-Host "==> Downloading agent from $env:TETHER_AGENT_URL"
  Invoke-WebRequest -Uri $env:TETHER_AGENT_URL -OutFile $Bin
} else {
  Write-Error "Set TETHER_AGENT_BIN or TETHER_AGENT_URL to the agent binary and re-run."
  exit 1
}

# 3. Enroll and register the logon task.
& $Bin enroll --controller $Controller --token $Token
& $Bin install

Write-Host "==> Done. __NAME__ is now reachable from your Tether controller."
"#;
