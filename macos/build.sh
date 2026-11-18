#!/bin/bash
# Builds KVM.app (universal: Apple Silicon + Intel) into ../dist and zips it.
set -euo pipefail

cd "$(dirname "$0")"
ROOT="$(cd .. && pwd)"
DIST="$ROOT/dist"
APP="$DIST/KVM.app"
VERSION="0.14.0"

source "$HOME/.cargo/env" 2>/dev/null || true

# Sign with the local self-signed certificate when it exists, so macOS keeps the
# Accessibility / Input Monitoring grants across builds. Ad-hoc otherwise.
SIGN_ID="$(security find-identity -p codesigning 2>/dev/null | awk '/"KVM Local Signing"/ {print $2; exit}')"
if [ -z "$SIGN_ID" ]; then
    echo "warning: 'KVM Local Signing' certificate not found, signing ad-hoc (permissions reset every build)"
    SIGN_ID="-"
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
codesign --force --sign "$SIGN_ID" --identifier "com.marcogaletta.kvm.awdl-helper" "$APP/Contents/Resources/kvm-awdl-helper"
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>com.marcogaletta.kvm</string>
    <key>CFBundleName</key><string>KVM</string>
    <key>CFBundleExecutable</key><string>KVM</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSUIElement</key><true/>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSLocalNetworkUsageDescription</key><string>KVM connects directly to your other computer on the local network.</string>
    <key>NSBonjourServices</key><array><string>_irohv1._udp</string></array>
</dict>
</plist>
EOF
codesign --force --sign "$SIGN_ID" --identifier com.marcogaletta.kvm "$APP"

cd "$DIST"
rm -f KVM-macos.zip
ditto -c -k --keepParent KVM.app KVM-macos.zip
echo "==> $APP"
echo "==> $DIST/KVM-macos.zip"
