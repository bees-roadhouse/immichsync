#!/bin/bash
# Build ImmichSync.app (and a zip of it) from this checkout.
#
#   packaging/macos/make-app.sh [out-dir]        # default: target/macos
#
# Signing:
#   MACOS_SIGN_IDENTITY  unset -> ad-hoc signature ("-"), fine for local use;
#                        Gatekeeper will refuse a downloaded copy.
#                        set   -> e.g. "Developer ID Application: ... (TEAMID)",
#                        hardened runtime + timestamp.
#   MACOS_NOTARY_PROFILE set (with a Developer ID) -> notarize the zip via
#                        `xcrun notarytool --keychain-profile`, then staple.
#
# Needs Xcode command-line tools (iconutil, codesign, ditto, plutil). No sudo.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${1:-$ROOT/target/macos}"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || { echo "could not read version from Cargo.toml" >&2; exit 1; }

# Build, and learn where build.rs put the iconset from cargo's own messages.
MSGS="$(cargo build --release --locked --message-format=json-render-diagnostics)"
BIN="$(printf '%s\n' "$MSGS" | jq -r 'select(.reason=="compiler-artifact" and .target.name=="immichsync" and .executable!=null) | .executable' | tail -1)"
ICONSET="$(printf '%s\n' "$MSGS" | jq -r 'select(.reason=="build-script-executed" and (.package_id|test("immichsync"))) | .out_dir' | tail -1)/icons/AppIcon.iconset"
[ -x "$BIN" ] || { echo "release binary not found" >&2; exit 1; }
[ -d "$ICONSET" ] || { echo "iconset not found at $ICONSET" >&2; exit 1; }

APP="$OUT/ImmichSync.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/immichsync"
sed "s/@VERSION@/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist" >/dev/null
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

IDENTITY="${MACOS_SIGN_IDENTITY:--}"
if [ "$IDENTITY" = "-" ]; then
  codesign --force --sign - "$APP"
else
  codesign --force --options runtime --timestamp \
    --entitlements packaging/macos/ImmichSync.entitlements --sign "$IDENTITY" "$APP"
fi
codesign --verify --strict "$APP"

ZIP="$OUT/ImmichSync-$VERSION-macos-arm64.zip"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"

if [ -n "${MACOS_NOTARY_PROFILE:-}" ] && [ "$IDENTITY" != "-" ]; then
  xcrun notarytool submit "$ZIP" --keychain-profile "$MACOS_NOTARY_PROFILE" --wait
  xcrun stapler staple "$APP"
  rm -f "$ZIP"
  ditto -c -k --keepParent "$APP" "$ZIP"
fi

echo "app: $APP"
echo "zip: $ZIP"
