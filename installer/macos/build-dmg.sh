#!/usr/bin/env bash
# Crea OmniLoad.app y su .dmg (sin firmar) para macOS.
# Uso: installer/macos/build-dmg.sh <versión> <arquitectura: arm64|x64> <carpeta de binarios>
set -euo pipefail

VERSION="${1:?falta la versión}"
ARCH="${2:?falta la arquitectura (arm64 o x64)}"
BIN="${3:?falta la carpeta con omniload, omnid y omni}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$ROOT/dist"
APP="$OUT/OmniLoad.app"
mkdir -p "$OUT"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# Los tres programas juntos: la ventana busca omnid en su misma carpeta.
cp "$BIN/omniload" "$BIN/omnid" "$BIN/omni" "$APP/Contents/MacOS/"

# Icono: PNG de 256 px → .icns
ICONSET="$(mktemp -d)/omniload.iconset"
mkdir -p "$ICONSET"
SRC="$ROOT/apps/omni-gui/ui/icon.png"
for s in 16 32 128 256; do
  sips -z $s $s "$SRC" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s*2)) $((s*2)) "$SRC" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/omniload.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>OmniLoad</string>
  <key>CFBundleDisplayName</key><string>OmniLoad</string>
  <key>CFBundleIdentifier</key><string>com.github.danijimzzz.omniload</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleExecutable</key><string>omniload</string>
  <key>CFBundleIconFile</key><string>omniload</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST

# Firma «ad hoc» (sin cuenta de desarrollador): evita que macOS diga que
# el programa está dañado. La primera vez hay que abrirlo con clic derecho.
codesign --force --deep --sign - "$APP"

STAGE="$(mktemp -d)"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
DMG="$OUT/OmniLoad-${VERSION}-macos-${ARCH}.dmg"
rm -f "$DMG"
hdiutil create -volname "OmniLoad" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$APP"
ls -l "$OUT"
