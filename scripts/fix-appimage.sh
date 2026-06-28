#!/usr/bin/env bash
#
# Tauri/linuxdeploy builds the AppImage with its `.DirIcon` as a broken
# *absolute* symlink into the build directory. File-manager thumbnailers read
# `.DirIcon` straight out of the squashfs, so the broken link means the
# AppImage shows no icon.
#
# This script repacks each built AppImage with `.DirIcon` replaced by a real
# copy of the 512px icon. The running-app/taskbar icon is handled separately by
# the embedded window icon (see `bundle.icon` order in tauri.conf.json).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUNDLE_DIR="$PROJECT_DIR/target/release/bundle/appimage"
ICON="$PROJECT_DIR/libretether-desktop/src-tauri/icons/icon.png"

shopt -s nullglob
appimages=("$BUNDLE_DIR"/*.AppImage)
if [ ${#appimages[@]} -eq 0 ]; then
  echo "fix-appimage: no AppImage in $BUNDLE_DIR — nothing to do."
  exit 0
fi

# appimagetool ships inside the linuxdeploy plugin Tauri downloads to its cache.
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/tauri"
PLUGIN="$CACHE/linuxdeploy-plugin-appimage.AppImage"
TOOLDIR="$CACHE/.appimagetool-extracted"
APPIMAGETOOL="$TOOLDIR/squashfs-root/usr/bin/appimagetool"
MKSQ_DIR="$TOOLDIR/squashfs-root/appimagetool-prefix/usr/bin"

if [ ! -x "$APPIMAGETOOL" ]; then
  if [ ! -f "$PLUGIN" ]; then
    echo "fix-appimage: appimagetool unavailable (run a full 'tauri build' first) — skipping." >&2
    exit 0
  fi
  rm -rf "$TOOLDIR"
  mkdir -p "$TOOLDIR"
  ( cd "$TOOLDIR" && "$PLUGIN" --appimage-extract >/dev/null )
fi

ARCH="${ARCH:-$(uname -m)}"
for img in "${appimages[@]}"; do
  echo "fix-appimage: repacking $(basename "$img") with a valid icon"
  workdir="$(mktemp -d)"
  ( cd "$workdir" && "$img" --appimage-extract >/dev/null )
  appdir="$workdir/squashfs-root"

  rm -f "$appdir/.DirIcon"
  cp "$ICON" "$appdir/.DirIcon"

  PATH="$MKSQ_DIR:$PATH" ARCH="$ARCH" NO_STRIP=1 \
    "$APPIMAGETOOL" --no-appstream "$appdir" "$img.fixed" >/dev/null 2>&1
  mv -f "$img.fixed" "$img"
  chmod +x "$img"
  rm -rf "$workdir"
done
echo "fix-appimage: done."
