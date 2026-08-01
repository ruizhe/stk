#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
GUI_BINARY=${1:-"$ROOT/crates/stk-gui/target/release/stk-gui"}
OUTPUT=${2:-}

if [ "$(uname -s)" != "Linux" ]; then
    echo "AppImage packaging must run on Linux" >&2
    exit 1
fi

case "$(uname -m)" in
    x86_64)
        TOOL_ARCH=x86_64
        LIB_ARCH=x86_64-linux-gnu
        APPIMAGE_RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
        APPIMAGE_RUNTIME_DIGEST_OFFSET=932096
        APPIMAGE_RUNTIME_DIGEST_SIZE=16
        ;;
    aarch64 | arm64)
        TOOL_ARCH=aarch64
        LIB_ARCH=aarch64-linux-gnu
        APPIMAGE_RUNTIME_SHA256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
        APPIMAGE_RUNTIME_DIGEST_OFFSET=923920
        APPIMAGE_RUNTIME_DIGEST_SIZE=16
        ;;
    *)
        echo "unsupported AppImage architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

if [ -z "$OUTPUT" ]; then
    OUTPUT="$ROOT/dist/stk-linux-$TOOL_ARCH.appimage"
fi

if [ ! -x "$GUI_BINARY" ]; then
    echo "required executable does not exist: $GUI_BINARY" >&2
    exit 1
fi

command -v curl >/dev/null 2>&1 || {
    echo "curl is required to download AppImage packaging tools" >&2
    exit 1
}
command -v file >/dev/null 2>&1 || {
    echo "file is required by appimagetool" >&2
    exit 1
}
command -v pkg-config >/dev/null 2>&1 || {
    echo "pkg-config is required by the linuxdeploy GTK plugin" >&2
    exit 1
}
pkg-config --exists gtk+-3.0 || {
    echo "GTK 3 development metadata is required to package the AppImage" >&2
    exit 1
}
pkg-config --exists ayatana-appindicator3-0.1 || {
    echo "Ayatana AppIndicator development metadata is required to package the AppImage" >&2
    exit 1
}

APPINDICATOR_LIB_DIR=$(pkg-config --variable=libdir ayatana-appindicator3-0.1)
APPINDICATOR_LIBRARY="$APPINDICATOR_LIB_DIR/libayatana-appindicator3.so.1"
if [ ! -e "$APPINDICATOR_LIBRARY" ]; then
    echo "Ayatana AppIndicator runtime library was not found: $APPINDICATOR_LIBRARY" >&2
    exit 1
fi

TOOLS_DIR=${STK_APPIMAGE_TOOLS_DIR:-"$ROOT/target/appimage-tools"}
APP_DIR="$ROOT/target/appimage/SSH_Tunnel_Keeper.AppDir"
DESKTOP_FILE="$APP_DIR/usr/share/applications/ssh-tunnel-keeper.desktop"
ICON_FILE="$APP_DIR/usr/share/icons/hicolor/256x256/apps/ssh-tunnel-keeper.png"
LINUXDEPLOY="$TOOLS_DIR/linuxdeploy-$TOOL_ARCH.AppImage"
GTK_PLUGIN="$TOOLS_DIR/linuxdeploy-plugin-gtk.sh"
APPRUN="$TOOLS_DIR/AppRun-$TOOL_ARCH"
APPIMAGETOOL="$TOOLS_DIR/appimagetool-$TOOL_ARCH-1.9.1.AppImage"
APPIMAGE_RUNTIME="$TOOLS_DIR/runtime-$TOOL_ARCH-20251108"

download_tool() {
    url=$1
    destination=$2
    if [ ! -s "$destination" ]; then
        mkdir -p "$(dirname -- "$destination")"
        temporary="$destination.download"
        if ! curl --fail --location \
            --connect-timeout 20 \
            --speed-limit 1024 \
            --speed-time 30 \
            --retry 5 \
            --retry-all-errors \
            --retry-delay 2 \
            --output "$temporary" "$url"
        then
            rm -f "$temporary"
            return 1
        fi
        mv "$temporary" "$destination"
    fi
    chmod 755 "$destination"
}

download_tool \
    "https://github.com/tauri-apps/binary-releases/releases/download/linuxdeploy/linuxdeploy-$TOOL_ARCH.AppImage" \
    "$LINUXDEPLOY"
download_tool \
    "https://raw.githubusercontent.com/tauri-apps/linuxdeploy-plugin-gtk/b5eb8d05b4c0ed40107fe2158c5d8527f94568ef/linuxdeploy-plugin-gtk.sh" \
    "$GTK_PLUGIN"
download_tool \
    "https://github.com/AppImage/AppImageKit/releases/download/continuous/AppRun-$TOOL_ARCH" \
    "$APPRUN"
download_tool \
    "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-$TOOL_ARCH.AppImage" \
    "$APPIMAGETOOL"
download_tool \
    "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-$TOOL_ARCH" \
    "$APPIMAGE_RUNTIME"

runtime_sha256=$(sha256sum "$APPIMAGE_RUNTIME" | awk '{print $1}')
if [ "$runtime_sha256" != "$APPIMAGE_RUNTIME_SHA256" ]; then
    echo "AppImage runtime checksum mismatch: $APPIMAGE_RUNTIME" >&2
    exit 1
fi

rm -rf "$APP_DIR"
mkdir -p \
    "$APP_DIR/usr/bin" \
    "$APP_DIR/usr/lib64" \
    "$APP_DIR/usr/share/doc/stk" \
    "$(dirname -- "$DESKTOP_FILE")" \
    "$(dirname -- "$ICON_FILE")"

install -m 755 "$GUI_BINARY" "$APP_DIR/usr/bin/stk-gui"
install -m 644 "$ROOT/packaging/linux/ssh-tunnel-keeper.desktop" "$DESKTOP_FILE"
install -m 644 "$ROOT/crates/stk-gui/assets/stk-icon-256.png" "$ICON_FILE"
install -m 644 "$ROOT/THIRD-PARTY-NOTICES.md" "$APP_DIR/usr/share/doc/stk/"
install -m 755 "$APPRUN" "$APP_DIR/AppRun"
ln -s "usr/share/applications/ssh-tunnel-keeper.desktop" \
    "$APP_DIR/ssh-tunnel-keeper.desktop"
ln -s "usr/share/icons/hicolor/256x256/apps/ssh-tunnel-keeper.png" \
    "$APP_DIR/ssh-tunnel-keeper.png"
ln -s "ssh-tunnel-keeper.png" "$APP_DIR/.DirIcon"

# WebKitGTK starts helper processes by path, so they must be copied explicitly.
for relative in \
    WebKitNetworkProcess \
    WebKitWebProcess \
    injected-bundle/libwebkit2gtkinjectedbundle.so
do
    found=false
    for base in \
        "/usr/lib/$LIB_ARCH" \
        /usr/lib64 \
        /usr/lib \
        /usr/libexec
    do
        source_file="$base/webkit2gtk-4.1/$relative"
        if [ -e "$source_file" ]; then
            destination="$APP_DIR$source_file"
            mkdir -p "$(dirname -- "$destination")"
            cp -L "$source_file" "$destination"
            found=true
            break
        fi
    done
    if [ "$found" != true ]; then
        echo "required WebKitGTK helper was not found: $relative" >&2
        exit 1
    fi
done

mkdir -p "$(dirname -- "$OUTPUT")"
OUTPUT_DIR=$(CDPATH= cd -- "$(dirname -- "$OUTPUT")" && pwd)
OUTPUT="$OUTPUT_DIR/$(basename -- "$OUTPUT")"
rm -f "$OUTPUT"

PATH="$TOOLS_DIR:$PATH"
export PATH
export ARCH="$TOOL_ARCH"
export DEPLOY_GTK_VERSION=3

"$LINUXDEPLOY" \
    --appimage-extract-and-run \
    --verbosity 2 \
    --appdir "$APP_DIR" \
    --library "$APPINDICATOR_LIBRARY" \
    --plugin gtk

if [ ! -e "$APP_DIR/usr/lib/libayatana-appindicator3.so.1" ]; then
    echo "linuxdeploy did not bundle libayatana-appindicator3.so.1" >&2
    exit 1
fi

"$APPIMAGETOOL" \
    --appimage-extract-and-run \
    --runtime-file "$APPIMAGE_RUNTIME" \
    "$APP_DIR" \
    "$OUTPUT"

if [ ! -x "$OUTPUT" ]; then
    echo "appimagetool did not create the expected AppImage: $OUTPUT" >&2
    exit 1
fi

runtime_size=$(wc -c < "$APPIMAGE_RUNTIME" | tr -d ' ')
runtime_tail_offset=$((APPIMAGE_RUNTIME_DIGEST_OFFSET + APPIMAGE_RUNTIME_DIGEST_SIZE))
runtime_tail_size=$((runtime_size - runtime_tail_offset))

# appimagetool writes the completed AppImage MD5 into the runtime's
# .digest_md5 section. Everything before and after that field must still
# match the pinned static runtime exactly.
if ! cmp -s -n "$APPIMAGE_RUNTIME_DIGEST_OFFSET" "$APPIMAGE_RUNTIME" "$OUTPUT" || \
    ! cmp -s -i "$runtime_tail_offset" -n "$runtime_tail_size" \
        "$APPIMAGE_RUNTIME" "$OUTPUT"
then
    echo "generated AppImage does not contain the selected static runtime" >&2
    exit 1
fi

printf '%s\n' "$OUTPUT"
