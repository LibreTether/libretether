#!/usr/bin/env sh
# LibreTether agent installer for macOS.
#
# Relay mode:
#   curl -fsSL <base>/install-macos.sh | sh -s -- \
#     --token TOKEN --relay HOST:PORT --relay-secret SECRET
#
# Direct / Tailscale mode:
#   curl -fsSL <base>/install-macos.sh | sh -s -- \
#     --token TOKEN --controller HOST:PORT [--tailscale-key KEY]
#
# Overrides: LIBRETETHER_AGENT_BIN=/path/to/binary or LIBRETETHER_AGENT_URL=https://...
set -eu

# The release workflow rewrites this to the exact repo + tag it publishes from.
RELEASE_BASE="https://github.com/LibreTether/libretether/releases/latest/download"

# Verify a downloaded file against its published <url>.sha256 sidecar. A custom
# LIBRETETHER_AGENT_URL may have no sidecar; in that case we warn and continue.
verify_checksum() {
	url="$1"; file="$2"
	expected="$(curl -fsSL "$url.sha256" 2>/dev/null | tr -d '[:space:]')" || true
	if [ -z "$expected" ]; then
		echo "==> No published checksum for $url — skipping integrity check." >&2
		return 0
	fi
	actual="$(shasum -a 256 "$file" | awk '{print $1}')"
	if [ "$expected" != "$actual" ]; then
		echo "!! Checksum mismatch for the downloaded agent (expected $expected, got $actual). Aborting." >&2
		rm -f "$file"
		exit 1
	fi
	echo "==> Verified agent checksum."
}

TOKEN=""
CONTROLLER=""
RELAY=""
RELAY_SECRET=""
TAILSCALE_KEY=""
CONTROLLER_KEY=""
NAME="$(hostname 2>/dev/null || echo this-mac)"

usage() {
	echo "usage: install-macos.sh --token TOKEN (--relay HOST:PORT --relay-secret SECRET | --controller HOST:PORT [--tailscale-key KEY]) [--name NAME]" >&2
}

while [ $# -gt 0 ]; do
	case "$1" in
		--token) TOKEN="$2"; shift 2 ;;
		--controller) CONTROLLER="$2"; shift 2 ;;
		--relay) RELAY="$2"; shift 2 ;;
		--relay-secret) RELAY_SECRET="$2"; shift 2 ;;
		--tailscale-key) TAILSCALE_KEY="$2"; shift 2 ;;
		--controller-key) CONTROLLER_KEY="$2"; shift 2 ;;
		--name) NAME="$2"; shift 2 ;;
		--agent-url) LIBRETETHER_AGENT_URL="$2"; shift 2 ;;
		-h|--help) usage; exit 0 ;;
		*) echo "!! unknown argument: $1" >&2; usage; exit 1 ;;
	esac
done

[ -n "$TOKEN" ] || { echo "!! --token is required" >&2; usage; exit 1; }
if [ -n "$RELAY" ] && [ -n "$CONTROLLER" ]; then
	echo "!! use --relay or --controller, not both" >&2; exit 1
fi
if [ -n "$RELAY" ]; then
	[ -n "$RELAY_SECRET" ] || { echo "!! --relay requires --relay-secret" >&2; exit 1; }
elif [ -z "$CONTROLLER" ]; then
	echo "!! provide --relay HOST:PORT --relay-secret SECRET, or --controller HOST:PORT" >&2; exit 1
fi

BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/libretether-agent"
echo "==> LibreTether agent install for $NAME"

# 1. Tailscale (direct mode with a pre-auth key only).
if [ -n "$TAILSCALE_KEY" ]; then
	[ -n "$CONTROLLER" ] || { echo "!! --tailscale-key only applies with --controller" >&2; exit 1; }
	if ! command -v tailscale >/dev/null 2>&1; then
		echo "!! Install Tailscale from https://tailscale.com/download/mac, then re-run." >&2; exit 1
	fi
	sudo tailscale up --reset --authkey "$TAILSCALE_KEY"
fi

# 2. Stop any running agent, then download the universal binary. Replacing a
#    running executable in place can fail, so unload the old LaunchAgent first;
#    `install` below reloads it. (Label mirrors libretether-agent/src/service.rs.)
launchctl unload "$HOME/Library/LaunchAgents/com.libretether.agent.plist" 2>/dev/null || true
mkdir -p "$BIN_DIR"
if [ -n "${LIBRETETHER_AGENT_BIN:-}" ]; then
	install -m 0755 "$LIBRETETHER_AGENT_BIN" "$BIN"
else
	URL="${LIBRETETHER_AGENT_URL:-$RELEASE_BASE/libretether-agent-macos-universal}"
	echo "==> Downloading agent from $URL"
	curl -fsSL "$URL" -o "$BIN"
	verify_checksum "$URL" "$BIN"
	chmod +x "$BIN"
fi

# 3. Enroll and install the LaunchAgent. The controller key (when supplied) pins
#    the controller identity so the agent only accepts that controller.
if [ -n "$CONTROLLER_KEY" ]; then set -- --controller-key "$CONTROLLER_KEY"; else set --; fi
if [ -n "$RELAY" ]; then
	"$BIN" enroll --relay "$RELAY" --relay-secret "$RELAY_SECRET" --token "$TOKEN" "$@"
else
	"$BIN" enroll --controller "$CONTROLLER" --token "$TOKEN" "$@"
fi
"$BIN" install

echo "==> Done. Grant Screen Recording + Accessibility to libretether-agent in System Settings > Privacy."
