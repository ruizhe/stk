#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
ASSETS="$ROOT/crates/stk-gui/assets"
SVG="$ASSETS/stk-icon.svg"
PNG="$ASSETS/stk-icon-1024.png"
WINDOW_PNG="$ASSETS/stk-icon-64.png"
LINUX_PNG="$ASSETS/stk-icon-256.png"
ICO="$ASSETS/stk-icon.ico"
TRAY_SVG="$ASSETS/stk-tray-icon.svg"
TRAY_PNG="$ASSETS/stk-tray-icon.png"
ICNS="$ROOT/crates/stk-gui/macos/AppIcon.icns"
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/stk-icons.XXXXXX")
ICONSET="$WORK_DIR/AppIcon.iconset"
mkdir "$ICONSET"

cleanup() {
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT

sips -s format png "$SVG" --out "$PNG" >/dev/null
sips -z 64 64 "$PNG" --out "$WINDOW_PNG" >/dev/null
sips -z 256 256 "$PNG" --out "$LINUX_PNG" >/dev/null

sips -z 16 16 "$PNG" --out "$ICONSET/icon_16x16.png" >/dev/null
sips -z 32 32 "$PNG" --out "$ICONSET/icon_16x16@2x.png" >/dev/null
sips -z 32 32 "$PNG" --out "$ICONSET/icon_32x32.png" >/dev/null
sips -z 64 64 "$PNG" --out "$ICONSET/icon_32x32@2x.png" >/dev/null
sips -z 128 128 "$PNG" --out "$ICONSET/icon_128x128.png" >/dev/null
sips -z 256 256 "$PNG" --out "$ICONSET/icon_128x128@2x.png" >/dev/null
sips -z 256 256 "$PNG" --out "$ICONSET/icon_256x256.png" >/dev/null
sips -z 512 512 "$PNG" --out "$ICONSET/icon_256x256@2x.png" >/dev/null
sips -z 512 512 "$PNG" --out "$ICONSET/icon_512x512.png" >/dev/null
cp "$PNG" "$ICONSET/icon_512x512@2x.png"
iconutil -c icns "$ICONSET" -o "$ICNS"

sips -s format ico "$LINUX_PNG" --out "$ICO" >/dev/null
sips -s format png "$TRAY_SVG" --out "$ICONSET/stk-tray-icon.png" >/dev/null
sips -z 22 22 "$ICONSET/stk-tray-icon.png" --out "$TRAY_PNG" >/dev/null

printf '%s\n' "$PNG" "$WINDOW_PNG" "$LINUX_PNG" "$ICNS" "$ICO" "$TRAY_PNG"
