#!/bin/bash
# Builds KVM.app (universal: Apple Silicon + Intel) into ../dist and zips it.
set -euo pipefail

cd "$(dirname "$0")"
ROOT="$(cd .. && pwd)"
DIST="$ROOT/dist"
APP="$DIST/KVM.app"
VERSION="0.15.0"

source "$HOME/.cargo/env" 2>/dev/null || true

# Signing, best available first:
# - "Developer ID Application": hardened runtime + timestamp, then notarized (if the
#   notarytool profile below is stored) so anyone can open it without warnings;
# - "KVM Local Signing" (self-signed): keeps permission grants across builds on this Mac;
# - ad-hoc: permissions reset every build.
# Store notarization credentials once with:
#   xcrun notarytool store-credentials kvm-notary --apple-id <you> --team-id CNAFXD6T86
NOTARY_PROFILE="${NOTARY_PROFILE:-kvm-notary}"
identity() { security find-identity -v -p codesigning 2>/dev/null | awk -v n="$1" 'index($0, n) {print $2; exit}'; }
SIGN_ID="$(identity '"Developer ID Application:')"
SIGN_FLAGS=()
if [ -n "$SIGN_ID" ]; then
    DISTRIBUTION=1
    SIGN_FLAGS=(--options runtime --timestamp)
else
    DISTRIBUTION=0
    SIGN_ID="$(security find-identity -p codesigning 2>/dev/null | awk '/"KVM Local Signing"/ {print $2; exit}')"
    if [ -z "$SIGN_ID" ]; then
        echo "warning: no signing certificate found, signing ad-hoc (permissions reset every build)"
        SIGN_ID="-"
    fi
fi

echo "==> Rust core"
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    MACOSX_DEPLOYMENT_TARGET=13.0 cargo build --release -p kvm-ffi --target "$target" --manifest-path "$ROOT/Cargo.toml"
done
mkdir -p "$ROOT/target/universal"
lipo -create \
    "$ROOT/target/aarch64-apple-darwin/release/libkvm_ffi.a" \
    "$ROOT/target/x86_64-apple-darwin/release/libkvm_ffi.a" \
    -output "$ROOT/target/universal/libkvm_ffi.a"

echo "==> AWDL helper"
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    MACOSX_DEPLOYMENT_TARGET=13.0 cargo build --release -p kvm-awdl-helper --target "$target" --manifest-path "$ROOT/Cargo.toml"
done
lipo -create \
    "$ROOT/target/aarch64-apple-darwin/release/kvm-awdl-helper" \
    "$ROOT/target/x86_64-apple-darwin/release/kvm-awdl-helper" \
    -output "$ROOT/target/universal/kvm-awdl-helper"

echo "==> Swift app"
swift build -c release --arch arm64 --arch x86_64
BIN="$(swift build -c release --arch arm64 --arch x86_64 --show-bin-path)/KVM"

echo "==> Bundle"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/KVM"
cp "$ROOT/target/universal/kvm-awdl-helper" "$APP/Contents/Resources/kvm-awdl-helper"
cp Resources/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns" # icon/make-icon.swift
codesign --force --sign "$SIGN_ID" ${SIGN_FLAGS[@]+"${SIGN_FLAGS[@]}"} --identifier "com.marcogaletta.kvm.awdl-helper" \
    "$APP/Contents/Resources/kvm-awdl-helper"
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>com.marcogaletta.kvm</string>
    <key>CFBundleName</key><string>KVM</string>
    <key>CFBundleExecutable</key><string>KVM</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSUIElement</key><true/>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSLocalNetworkUsageDescription</key><string>KVM connects directly to your other computer on the local network.</string>
    <key>NSBonjourServices</key><array><string>_kvm._udp</string></array>
    <key>NSAppleEventsUsageDescription</key><string>KVM installs its Wi-Fi helper with your administrator password.</string>
</dict>
</plist>
EOF
codesign --force --sign "$SIGN_ID" ${SIGN_FLAGS[@]+"${SIGN_FLAGS[@]}"} --entitlements KVM.entitlements \
    --identifier com.marcogaletta.kvm "$APP"
codesign --verify --deep --strict "$APP"

cd "$DIST"
rm -f KVM-macos.zip
ditto -c -k --keepParent KVM.app KVM-macos.zip

if [ "$DISTRIBUTION" = 1 ]; then
    if xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
        echo "==> Notarizing (usually a few minutes)"
        RESULT="$(xcrun notarytool submit KVM-macos.zip --keychain-profile "$NOTARY_PROFILE" --wait --output-format json)"
        STATUS="$(printf '%s' "$RESULT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("status",""))')"
        if [ "$STATUS" != "Accepted" ]; then
            ID="$(printf '%s' "$RESULT" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')"
            echo "error: notarization $STATUS. Apple's log:"
            xcrun notarytool log "$ID" --keychain-profile "$NOTARY_PROFILE" | python3 -c '
import json, sys
d = json.load(sys.stdin)
print(" ", d.get("statusSummary"))
for i in d.get("issues") or []:
    print("  -", i.get("path"), ":", i.get("message"))'
            exit 1
        fi
        xcrun stapler staple KVM.app
        rm -f KVM-macos.zip
        ditto -c -k --keepParent KVM.app KVM-macos.zip
        spctl --assess --type execute --verbose KVM.app
    else
        echo "warning: Developer ID signed but not notarized: no '$NOTARY_PROFILE' notarytool profile"
    fi
fi
echo "==> $APP"
echo "==> $DIST/KVM-macos.zip"
