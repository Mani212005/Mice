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

# `--check`: report exactly what the Developer-ID signing/notarization path
# still needs on this machine, without building anything.
if [[ "${1:-}" == "--check" ]]; then
    status=0
    echo "==> Developer-ID packaging prerequisites"
    identities="$(security find-identity -v -p codesigning 2>/dev/null | grep 'Developer ID Application' || true)"
    if [[ -n "$identities" ]]; then
        echo "ok   Developer ID Application identity present:"
        echo "$identities" | sed 's/^/       /'
    else
        echo "MISSING  No 'Developer ID Application' identity in the keychain."
        echo "         Enroll at developer.apple.com, create a Developer ID Application"
        echo "         certificate in Xcode (Settings > Accounts > Manage Certificates)"
        echo "         or the developer portal, and install it in the login keychain."
        status=1
    fi
    if xcrun --find notarytool >/dev/null 2>&1; then
        echo "ok   notarytool available: $(xcrun --find notarytool)"
    else
        echo "MISSING  notarytool (install Xcode or Command Line Tools)."
        status=1
    fi
    if xcrun --find stapler >/dev/null 2>&1; then
        echo "ok   stapler available: $(xcrun --find stapler)"
    else
        echo "MISSING  stapler (install Xcode or Command Line Tools)."
        status=1
    fi
    profile="${MICE_NOTARY_PROFILE:-mice-notary}"
    if xcrun notarytool history --keychain-profile "$profile" >/dev/null 2>&1; then
        echo "ok   notarytool keychain profile '$profile' works."
    else
        echo "MISSING  notarytool keychain profile '$profile'."
        echo "         Create it once with an App Store Connect API key or app-specific"
        echo "         password:  xcrun notarytool store-credentials $profile \\"
        echo "                      --apple-id <appleid> --team-id <TEAMID> --password <app-specific>"
        status=1
    fi
    if [[ $status -eq 0 ]]; then
        echo "All prerequisites present. Run:"
        echo "  MICE_SIGNING_IDENTITY=\"Developer ID Application: …\" MICE_NOTARY_PROFILE=$profile scripts/package-macos.sh"
    else
        echo "Prerequisites missing (see above). Unsigned/ad-hoc packaging still works: scripts/package-macos.sh"
    fi
    exit $status
fi

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
    echo "==> Validating the notarized bundle"
    xcrun stapler validate "$APP"
    # The Gatekeeper assessment a recipient's Mac will perform.
    spctl --assess --type execute --verbose "$APP"
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
Install once (no administrator password or shell-profile edit required):
  "$APP/Contents/MacOS/mice" install
Then, from any folder:
  mice
First run: grant Accessibility, Screen Recording, and Input Monitoring to
MICE.app in System Settings > Privacy & Security when prompted.
NOTES
