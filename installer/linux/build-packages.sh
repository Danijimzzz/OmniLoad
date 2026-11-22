#!/usr/bin/env bash
# Crea los paquetes de Linux: .deb, .tar.gz y .AppImage.
# Uso: installer/linux/build-packages.sh <versión>   (tras compilar en release)
set -euo pipefail

VERSION="${1:?falta la versión, por ejemplo 0.4.0}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$ROOT/target/release"
OUT="$ROOT/dist"
ARCH_DEB="amd64"
NAME="OmniLoad-${VERSION}-linux-x64"
mkdir -p "$OUT"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# --- .deb -------------------------------------------------------------
DEB="$WORK/deb"
install -Dm755 "$BIN/omniload" "$DEB/usr/bin/omniload"
install -Dm755 "$BIN/omnid" "$DEB/usr/bin/omnid"
install -Dm755 "$BIN/omni" "$DEB/usr/bin/omni"
install -Dm644 "$ROOT/installer/linux/omniload.desktop" "$DEB/usr/share/applications/omniload.desktop"
install -Dm644 "$ROOT/apps/omni-gui/ui/icon.png" "$DEB/usr/share/icons/hicolor/256x256/apps/omniload.png"
install -Dm644 "$ROOT/LICENSE" "$DEB/usr/share/doc/omniload/copyright"
mkdir -p "$DEB/DEBIAN"
cat > "$DEB/DEBIAN/control" <<CONTROL
Package: omniload
Version: ${VERSION}
Section: net
Priority: optional
Architecture: ${ARCH_DEB}
Depends: libfontconfig1, libxkbcommon-x11-0
Recommends: yt-dlp, ffmpeg
Maintainer: OmniLoad <https://github.com/Danijimzzz/OmniLoad>
Homepage: https://github.com/Danijimzzz/OmniLoad
Description: Gestor de descargas libre y rápido
 OmniLoad descarga por varios trozos a la vez, reanuda lo que se corta,
 comprueba los enlaces antes de bajarlos y entiende Mega, Google Drive,
 Mediafire, Gofile, torrents y mucho más.
CONTROL
dpkg-deb --build --root-owner-group "$DEB" "$OUT/${NAME}.deb"

# --- .tar.gz ----------------------------------------------------------
TAR="$WORK/${NAME}"
mkdir -p "$TAR"
cp "$BIN/omniload" "$BIN/omnid" "$BIN/omni" "$ROOT/LICENSE" "$TAR/"
cp "$ROOT/installer/linux/omniload.desktop" "$ROOT/apps/omni-gui/ui/icon.png" "$TAR/"
tar -C "$WORK" -czf "$OUT/${NAME}.tar.gz" "${NAME}"

# --- .AppImage --------------------------------------------------------
# La app comprueba $APPIMAGE para actualizarse a sí misma.
APPDIR="$WORK/OmniLoad.AppDir"
install -Dm755 "$BIN/omniload" "$APPDIR/usr/bin/omniload"
install -Dm755 "$BIN/omnid" "$APPDIR/usr/bin/omnid"
install -Dm755 "$BIN/omni" "$APPDIR/usr/bin/omni"
install -Dm644 "$ROOT/installer/linux/omniload.desktop" "$APPDIR/omniload.desktop"
install -Dm644 "$ROOT/apps/omni-gui/ui/icon.png" "$APPDIR/omniload.png"
cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
export PATH="$HERE/usr/bin:$PATH"
exec "$HERE/usr/bin/omniload" "$@"
APPRUN
chmod +x "$APPDIR/AppRun"
TOOL="$WORK/appimagetool"
curl -fsSL -o "$TOOL" \
  "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage"
chmod +x "$TOOL"
ARCH=x86_64 "$TOOL" --appimage-extract-and-run "$APPDIR" "$OUT/${NAME}.AppImage"

ls -l "$OUT"
