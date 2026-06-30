#!/usr/bin/env sh
# LibreTether agent installer for Linux.
#
# Relay mode:
#   curl -fsSL <base>/install-linux.sh | sh -s -- \
#     --token TOKEN --relay HOST:PORT --relay-secret SECRET
#
# Direct / Tailscale mode:
#   curl -fsSL <base>/install-linux.sh | sh -s -- \
#     --token TOKEN --controller HOST:PORT [--tailscale-key KEY]
#
# Overrides: LIBRETETHER_AGENT_BIN=/path/to/binary or LIBRETETHER_AGENT_URL=https://...
# Note: the runtime-library and Tailscale steps use sudo; when piping into `sh`,
# either run as root or pre-authenticate sudo first.
set -eu

# The release workflow rewrites this to the exact repo + tag it publishes from,
# so a versioned script always pulls the matching agent build.
RELEASE_BASE="https://github.com/LibreTether/libretether/releases/latest/download"

# Verify a downloaded file against its published <url>.sha256 sidecar. `curl --retry`
# rides out a transient fetch failure (5xx/timeout) so a network blip isn't
# mistaken for an absent checksum — only a genuine 404 (no sidecar) falls through
# below. The official release always ships a sidecar, so its absence there is a hard
# failure ($required=1 — fail closed rather than install an unverified agent). A custom
# LIBRETETHER_AGENT_URL may legitimately have none; for that ($required=0) we warn on.
verify_checksum() {
	url="$1"; file="$2"; required="$3"
	expected="$(curl -fsSL --retry 3 --retry-delay 2 "$url.sha256" 2>/dev/null | tr -d '[:space:]')" || true
	if [ -z "$expected" ]; then
		if [ "$required" = 1 ]; then
			echo "!! No published checksum for $url — refusing to install an unverified agent. Aborting." >&2
			rm -f "$file"
			exit 1
		fi
		echo "==> No published checksum for $url (custom URL) — skipping integrity check." >&2
		return 0
	fi
	actual="$(sha256sum "$file" | awk '{print $1}')"
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
CODE=""
NAME="$(hostname 2>/dev/null || echo this-machine)"

usage() {
	echo "usage: install-linux.sh (--pair --relay HOST:PORT --code CODE | --token TOKEN (--relay HOST:PORT --relay-secret SECRET | --controller HOST:PORT [--tailscale-key KEY])) [--name NAME]" >&2
}

while [ $# -gt 0 ]; do
	case "$1" in
		--token) TOKEN="$2"; shift 2 ;;
		--controller) CONTROLLER="$2"; shift 2 ;;
		--relay) RELAY="$2"; shift 2 ;;
		--relay-secret) RELAY_SECRET="$2"; shift 2 ;;
		--tailscale-key) TAILSCALE_KEY="$2"; shift 2 ;;
		--controller-key) CONTROLLER_KEY="$2"; shift 2 ;;
		--code) CODE="$2"; shift 2 ;;
		--pair) shift ;;  # marker; pair mode is implied by --code
		--name) NAME="$2"; shift 2 ;;
		--agent-url) LIBRETETHER_AGENT_URL="$2"; shift 2 ;;
		-h|--help) usage; exit 0 ;;
		*) echo "!! unknown argument: $1" >&2; usage; exit 1 ;;
	esac
done

# Pairing mode (a short code from the browser portal) needs only the relay; the
# token, secret and controller key all arrive over the PAKE channel. Otherwise it's
# classic enrollment with a token.
if [ -n "$CODE" ]; then
	[ -n "$RELAY" ] || { echo "!! --code requires --relay HOST:PORT" >&2; usage; exit 1; }
else
	[ -n "$TOKEN" ] || { echo "!! --token is required (or use --pair --code for a portal code)" >&2; usage; exit 1; }
	if [ -n "$RELAY" ] && [ -n "$CONTROLLER" ]; then
		echo "!! use --relay or --controller, not both" >&2; exit 1
	fi
	if [ -n "$RELAY" ]; then
		[ -n "$RELAY_SECRET" ] || { echo "!! --relay requires --relay-secret" >&2; exit 1; }
	elif [ -z "$CONTROLLER" ]; then
		echo "!! provide --relay HOST:PORT --relay-secret SECRET, or --controller HOST:PORT" >&2; exit 1
	fi
fi

BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/libretether-agent"
echo "==> LibreTether agent install for $NAME"

# 1. Tailscale (direct mode with a pre-auth key only).
if [ -n "$TAILSCALE_KEY" ]; then
	[ -n "$CONTROLLER" ] || { echo "!! --tailscale-key only applies with --controller" >&2; exit 1; }
	if ! command -v tailscale >/dev/null 2>&1; then
		echo "==> Installing Tailscale"
		curl -fsSL https://tailscale.com/install.sh | sh
	fi
	sudo tailscale up --reset --authkey "$TAILSCALE_KEY"
fi

# 2. Runtime libraries the agent links against (X11 input/capture + PipeWire for
#    Wayland); gnome-remote-desktop is for the optional RDP path on Wayland.
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

# 3. Stop any running agent, then download the new binary. The kernel refuses to
#    overwrite a running executable in place ("text file busy"), so a re-deploy
#    must stop the old service first; `install` below re-enables and restarts it.
#    (Unit name mirrors libretether-agent/src/service.rs.)
if command -v systemctl >/dev/null 2>&1; then
	systemctl --user stop libretether-agent.service 2>/dev/null || true
fi
mkdir -p "$BIN_DIR"
if [ -n "${LIBRETETHER_AGENT_BIN:-}" ]; then
	install -m 0755 "$LIBRETETHER_AGENT_BIN" "$BIN"
else
	# A custom URL may lack a checksum sidecar (require=0); the official release
	# always has one (require=1).
	if [ -n "${LIBRETETHER_AGENT_URL:-}" ]; then
		URL="$LIBRETETHER_AGENT_URL"; REQUIRE_SUM=0
	else
		case "$(uname -m)" in
			x86_64|amd64) ARCH=x86_64 ;;
			aarch64|arm64) ARCH=aarch64 ;;
			*) echo "!! Unsupported architecture $(uname -m). Set LIBRETETHER_AGENT_BIN or LIBRETETHER_AGENT_URL." >&2; exit 1 ;;
		esac
		URL="$RELEASE_BASE/libretether-agent-linux-$ARCH"; REQUIRE_SUM=1
	fi
	echo "==> Downloading agent from $URL"
	curl -fsSL --retry 3 --retry-delay 2 "$URL" -o "$BIN"
	verify_checksum "$URL" "$BIN" "$REQUIRE_SUM"
	chmod +x "$BIN"
fi

# 4. Pair (portal code) or enroll (token), then install the always-on user service.
#    In pair mode the controller key + token arrive over the PAKE channel; in enroll
#    mode the controller key (when supplied) is pinned via the positional parameters.
if [ -n "$CODE" ]; then
	"$BIN" pair --relay "$RELAY" --code "$CODE"
else
	if [ -n "$CONTROLLER_KEY" ]; then set -- --controller-key "$CONTROLLER_KEY"; else set --; fi
	if [ -n "$RELAY" ]; then
		"$BIN" enroll --relay "$RELAY" --relay-secret "$RELAY_SECRET" --token "$TOKEN" "$@"
	else
		"$BIN" enroll --controller "$CONTROLLER" --token "$TOKEN" "$@"
	fi
fi
"$BIN" install

echo "==> Done. $NAME is now reachable from your LibreTether controller."
