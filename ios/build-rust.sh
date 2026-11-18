#!/bin/bash
# Builds the Rust core (kvm-ffi) for what Xcode is building; run by the "Build Rust core"
# phase. The library lands in target/ios/<platform>/ (LIBRARY_SEARCH_PATHS).
# Needs the targets once: rustup target add aarch64-apple-ios aarch64-apple-ios-sim
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PLATFORM="${PLATFORM_NAME:-iphoneos}"
case "$PLATFORM" in
    iphonesimulator) TARGET=aarch64-apple-ios-sim ;;
    iphoneos) TARGET=aarch64-apple-ios ;;
    *) echo "error: unsupported platform $PLATFORM"; exit 1 ;;
esac

# Xcode's environment (SDKROOT and friends point at iOS) breaks the host build scripts of
# the Rust dependencies, so cargo runs with a clean one.
env -i HOME="$HOME" PATH="$HOME/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
    ${DEVELOPER_DIR:+DEVELOPER_DIR="$DEVELOPER_DIR"} \
    IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-17.0}" \
    cargo build --release -p kvm-ffi --target "$TARGET" --manifest-path "$ROOT/Cargo.toml"

mkdir -p "$ROOT/target/ios/$PLATFORM"
cp "$ROOT/target/$TARGET/release/libkvm_ffi.a" "$ROOT/target/ios/$PLATFORM/libkvm_ffi.a"
