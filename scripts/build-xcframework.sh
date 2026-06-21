#!/usr/bin/env bash
# Build FreedomMobile.xcframework: the combined Swarm (ant-ffi) + IPFS
# (freedom-ipfs-mobile) Rust staticlib, packaged for iOS device +
# simulator with a union C header surface.
#
# Slices: aarch64-apple-ios (device), and a fat simulator slice
# (aarch64-apple-ios-sim + x86_64-apple-ios via lipo). Toolchain comes
# from rust-toolchain.toml. macOS only (needs xcodebuild).
set -euo pipefail

CRATE=freedom-mobile-ffi
LIBNAME=libfreedom_mobile_ffi.a
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/target/ios-xcframework"
HDRS="$OUT/headers"
FRAMEWORK="$OUT/FreedomMobile.xcframework"

ANT_HEADER="$ROOT/../ant/crates/ant-ffi/include/ant.h"
IPFS_HEADER="$ROOT/../freedom-ipfs/ffi/include/freedom_ipfs.h"

DEVICE=aarch64-apple-ios
SIM_ARM=aarch64-apple-ios-sim
SIM_X86=x86_64-apple-ios

echo "==> Building release staticlibs"
for t in "$DEVICE" "$SIM_ARM" "$SIM_X86"; do
  echo "    - $t"
  ( cd "$ROOT" && IPHONEOS_DEPLOYMENT_TARGET=18.0 \
      cargo build --release --target "$t" -p "$CRATE" )
done

echo "==> Staging union headers + modulemap"
rm -rf "$OUT"
mkdir -p "$HDRS"
cp "$ANT_HEADER" "$HDRS/ant.h"
cp "$IPFS_HEADER" "$HDRS/freedom_ipfs.h"
cp "$ROOT/include/module.modulemap" "$HDRS/module.modulemap"

echo "==> lipo fat simulator slice"
mkdir -p "$OUT/sim"
lipo -create -output "$OUT/sim/$LIBNAME" \
  "$ROOT/target/$SIM_ARM/release/$LIBNAME" \
  "$ROOT/target/$SIM_X86/release/$LIBNAME"

echo "==> Assembling xcframework"
xcodebuild -create-xcframework \
  -library "$ROOT/target/$DEVICE/release/$LIBNAME" -headers "$HDRS" \
  -library "$OUT/sim/$LIBNAME" -headers "$HDRS" \
  -output "$FRAMEWORK"

echo "==> Done: $FRAMEWORK"
