#!/bin/sh
set -eu

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
    printf 'usage: %s <app-bundle> <output.dmg> [volume-name]\n' "$0" >&2
    exit 2
fi

APP=$1
OUTPUT=$2
VOLUME_NAME=${3:-SSH Tunnel Keeper}

if [ ! -d "$APP/Contents" ]; then
    printf 'invalid macOS application bundle: %s\n' "$APP" >&2
    exit 1
fi

OUTPUT_DIR=$(dirname "$OUTPUT")
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/stk-dmg.XXXXXX")

cleanup() {
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT

mkdir -p "$OUTPUT_DIR"
ditto "$APP" "$WORK_DIR/SSH Tunnel Keeper.app"
ln -s /Applications "$WORK_DIR/Applications"

hdiutil create \
    -quiet \
    -volname "$VOLUME_NAME" \
    -srcfolder "$WORK_DIR" \
    -ov \
    -format UDZO \
    "$OUTPUT"
hdiutil verify "$OUTPUT" >/dev/null

printf '%s\n' "$OUTPUT"
