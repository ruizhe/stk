#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
MANIFEST="$ROOT/crates/stk-gui/Cargo.toml"
TARGET="$ROOT/crates/stk-gui/target/release"
APP="$TARGET/bundle/macos/SSH Tunnel Keeper.app"
CONTENTS="$APP/Contents"

cargo build --manifest-path "$MANIFEST" --features desktop --release

rm -rf "$APP"
install -d "$CONTENTS/MacOS" "$CONTENTS/Resources"
install -m 755 "$TARGET/stk-gui" "$CONTENTS/MacOS/stk-gui"
install -m 644 "$ROOT/crates/stk-gui/macos/Info.plist" "$CONTENTS/Info.plist"
install -m 644 "$ROOT/crates/stk-gui/macos/AppIcon.icns" "$CONTENTS/Resources/AppIcon.icns"

if command -v codesign >/dev/null 2>&1; then
    codesign --force --sign - "$APP" >/dev/null
fi

printf '%s\n' "$APP"
