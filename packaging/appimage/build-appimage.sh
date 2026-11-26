#!/usr/bin/env bash
# 构建 AppImage。正常由 CI 在 Arch 容器里调用；本地也可直接跑。
#
# 依赖：curl、patchelf、glib2（gschemas.compiled）、squashfs-tools；
# linuxdeploy / appimagetool 会自动下载到临时目录。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
VERSION="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)}"
OUT="${2:-$ROOT/dist}"
BIN="$ROOT/target/release/gh-proxy-manager"
DESKTOP="$ROOT/assets/io.github.duter2016.GhProxyManager.desktop"
ICON="$ROOT/assets/io.github.duter2016.GhProxyManager.svg"

if [ ! -x "$BIN" ]; then
    echo "缺少 $BIN，请先 cargo build --release --locked" >&2
    exit 1
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
APPDIR="$WORK/GhProxyManager.AppDir"
TOOLS="$WORK/tools"
mkdir -p "$APPDIR/usr/bin" \
         "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/scalable/apps" \
         "$APPDIR/usr/share/glib-2.0/schemas" \
         "$TOOLS" "$OUT"

install -Dm755 "$BIN" "$APPDIR/usr/bin/gh-proxy-manager"
install -Dm644 "$DESKTOP" \
    "$APPDIR/usr/share/applications/io.github.duter2016.GhProxyManager.desktop"
install -Dm644 "$ICON" \
    "$APPDIR/usr/share/icons/hicolor/scalable/apps/io.github.duter2016.GhProxyManager.svg"
install -Dm755 "$ROOT/packaging/appimage/AppRun" "$APPDIR/AppRun"
ln -sf usr/share/icons/hicolor/scalable/apps/io.github.duter2016.GhProxyManager.svg \
    "$APPDIR/.DirIcon"

# libadwaita 的 GSettings schema；缺了会有大量 critical。
if [ -f /usr/share/glib-2.0/schemas/gschemas.compiled ]; then
    cp /usr/share/glib-2.0/schemas/gschemas.compiled \
        "$APPDIR/usr/share/glib-2.0/schemas/"
fi

echo "下载 linuxdeploy / appimagetool …"
curl -fsSL --retry 3 -o "$TOOLS/linuxdeploy" \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage"
curl -fsSL --retry 3 -o "$TOOLS/appimagetool" \
    "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage"
chmod +x "$TOOLS/linuxdeploy" "$TOOLS/appimagetool"

export APPIMAGE_EXTRACT_AND_RUN=1
export ARCH=x86_64

# 收集 GTK4 / libadwaita 及其非系统依赖，写入 AppDir。
# linuxdeploy 失败时仍然产出一个“依赖主机 GTK”的 AppImage，不让整个发布失败。
if ! "$TOOLS/linuxdeploy" --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/gh-proxy-manager" \
    --desktop-file "$APPDIR/usr/share/applications/io.github.duter2016.GhProxyManager.desktop" \
    --icon-file "$APPDIR/usr/share/icons/hicolor/scalable/apps/io.github.duter2016.GhProxyManager.svg"; then
    echo "警告: linuxdeploy 失败，退回不捆绑 GTK 的 AppImage（要求主机已安装 gtk4/libadwaita）" >&2
fi

TARGET="$OUT/gh-proxy-manager-$VERSION-x86_64.AppImage"
"$TOOLS/appimagetool" --no-appstream "$APPDIR" "$TARGET"
echo "已生成: $TARGET"