#!/usr/bin/env bash
#
# Install the freshly-built desktop bundle for this host: the .deb on apt-based
# distros, the .rpm on dnf/yum-based ones, or the .app on macOS. The Cargo
# workspace writes bundles under the workspace-root target/, so this just picks
# the right one for the host. Run after `run desktop:build` (or via
# `run desktop:install`, which builds first).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUNDLE_DIR="$PROJECT_DIR/target/release/bundle"

# Newest package of the given kind ("deb"/"rpm"); the bundle subdir name and the
# file extension are the same. Empty (exit 0) when none exist.
latest() {
	ls -1t "$BUNDLE_DIR/$1"/*."$1" 2>/dev/null | head -n1 || true
}

if [ "$(uname)" = "Darwin" ]; then
	app="$(ls -1dt "$BUNDLE_DIR/macos"/*.app 2>/dev/null | head -n1 || true)"
	[ -n "$app" ] || { echo "No .app in $BUNDLE_DIR/macos — run 'run desktop:build' first." >&2; exit 1; }
	dest="/Applications/$(basename "$app")"
	echo "Installing $app to $dest…"
	rm -rf "$dest"
	cp -R "$app" /Applications/
elif command -v apt-get >/dev/null 2>&1; then
	pkg="$(latest deb)"
	[ -n "$pkg" ] || { echo "No .deb in $BUNDLE_DIR/deb — run 'run desktop:build' first." >&2; exit 1; }
	echo "Installing $pkg with apt-get…"
	sudo apt-get install -y "$pkg"
elif command -v dnf >/dev/null 2>&1; then
	pkg="$(latest rpm)"
	[ -n "$pkg" ] || { echo "No .rpm in $BUNDLE_DIR/rpm — run 'run desktop:build' first." >&2; exit 1; }
	echo "Installing $pkg with dnf…"
	sudo dnf install -y "$pkg"
elif command -v yum >/dev/null 2>&1; then
	pkg="$(latest rpm)"
	[ -n "$pkg" ] || { echo "No .rpm in $BUNDLE_DIR/rpm — run 'run desktop:build' first." >&2; exit 1; }
	echo "Installing $pkg with yum…"
	sudo yum install -y "$pkg"
else
	echo "No supported package manager found (need apt, dnf or yum)." >&2
	exit 1
fi
