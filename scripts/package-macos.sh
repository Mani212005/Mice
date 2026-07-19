#!/usr/bin/env bash
# Build a distributable MICE.app, DMG, and zip.
#
# Unsigned/dev:      scripts/package-macos.sh
# Signed:            MICE_SIGNING_IDENTITY="Developer ID Application: …" scripts/package-macos.sh
# Signed+notarized:  additionally MICE_NOTARY_PROFILE=<notarytool keychain profile>
#
# Without a Developer ID the bundle is ad-hoc signed: it runs locally (and TCC
# permission grants stick to the bundle), but Gatekeeper on other machines
# will require right-click → Open. Signing and notarization activate
# automatically when the credentials above are present — no script changes.
#
# Upgrade safety: user state (config.toml, tidy undo log, filing index, shared
# memory) lives in ~/Library/Application Support/MICE and survives replacing
# MICE.app. The CLI locates its agent beside its own binary, so an upgraded
# bundle can never mix a new CLI with an old agent.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
DIST="$ROOT/dist"
APP="$DIST/MICE.app"
BUNDLE_ID="com.mice.app"

echo "==> Building release binaries (v$VERSION)"
(cd "$ROOT" && cargo build --release -p mice-cli)
(cd "$ROOT/agent-macos" && swift build -c release)

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$ROOT/target/release/mice" "$APP/Contents/MacOS/mice"
cp "$ROOT/agent-macos/.build/release/mice-mac-agent" "$APP/Contents/MacOS/mice-mac-agent"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
    <key>CFBundleName</key><string>MICE</string>
    <key>CFBundleExecutable</key><string>mice</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSUIElement</key><true/>
    <key>NSHumanReadableCopyright</key><string>Apache-2.0</string>
</dict>
</plist>
PLIST

ENTITLEMENTS="$ROOT/scripts/mice.entitlements"
if [[ -n "${MICE_NOTARY_PROFILE:-}" ]]; then
    if [[ -z "${MICE_SIGNING_IDENTITY:-}" || "$MICE_SIGNING_IDENTITY" != Developer\ ID\ Application:* ]]; then
        echo "error: notarization requires MICE_SIGNING_IDENTITY='Developer ID Application: …'" >&2
        exit 2
    fi
fi
if [[ -n "${MICE_SIGNING_IDENTITY:-}" ]]; then
    echo "==> Codesigning with: $MICE_SIGNING_IDENTITY (hardened runtime)"
    codesign --force --options runtime --timestamp \
        --entitlements "$ENTITLEMENTS" -s "$MICE_SIGNING_IDENTITY" \
        "$APP/Contents/MacOS/mice-mac-agent"
    codesign --force --options runtime --timestamp \
        --entitlements "$ENTITLEMENTS" -s "$MICE_SIGNING_IDENTITY" \
        "$APP/Contents/MacOS/mice"
    codesign --force --options runtime --timestamp \
        --entitlements "$ENTITLEMENTS" -s "$MICE_SIGNING_IDENTITY" "$APP"
else
    echo "==> No MICE_SIGNING_IDENTITY set; applying ad-hoc signature (local use)"
    codesign --force -s - "$APP/Contents/MacOS/mice-mac-agent"
    codesign --force -s - "$APP/Contents/MacOS/mice"
    codesign --force -s - "$APP"
fi
codesign --verify --deep "$APP"

ZIP="$DIST/MICE-$VERSION.zip"
DMG="$DIST/MICE-$VERSION.dmg"
rm -f "$ZIP" "$DMG"

if [[ -n "${MICE_NOTARY_PROFILE:-}" ]]; then
    echo "==> Notarizing via keychain profile: $MICE_NOTARY_PROFILE"
    ditto -c -k --keepParent "$APP" "$DIST/MICE-notarize.zip"
    xcrun notarytool submit "$DIST/MICE-notarize.zip" \
        --keychain-profile "$MICE_NOTARY_PROFILE" --wait
    xcrun stapler staple "$APP"
    rm -f "$DIST/MICE-notarize.zip"
elif [[ -n "${MICE_SIGNING_IDENTITY:-}" ]]; then
    echo "==> Skipping notarization (set MICE_NOTARY_PROFILE to enable)"
fi

echo "==> Creating archives"
ditto -c -k --keepParent "$APP" "$ZIP"
hdiutil create -volname "MICE" -srcfolder "$APP" -ov -format UDZO -quiet "$DMG"

echo "==> Checksums"
(cd "$DIST" && shasum -a 256 "$(basename "$ZIP")" "$(basename "$DMG")")

cat <<NOTES

Done: $APP
Install: drag MICE.app to /Applications, then add the CLI to your shell:
  alias mice="/Applications/MICE.app/Contents/MacOS/mice"
First run: grant Accessibility, Screen Recording, and Input Monitoring to
MICE.app in System Settings > Privacy & Security when prompted.
NOTES
