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

DEVICE=aarch64-apple-ios
SIM_ARM=aarch64-apple-ios-sim
SIM_X86=x86_64-apple-ios

echo "==> Building release staticlibs"
for t in "$DEVICE" "$SIM_ARM" "$SIM_X86"; do
  echo "    - $t"
  ( cd "$ROOT" && IPHONEOS_DEPLOYMENT_TARGET=18.0 \
      cargo build --release --target "$t" -p "$CRATE" )
done

# Radicle's Swift surface is UniFFI, so its C header + Swift glue are
# GENERATED from the compiled library (bindgen/ is pinned to the same
# uniffi minor as the scaffolding; a skew is a build error by design).
# Library mode scans the device slice so the bindings can never drift
# from the symbols actually shipped.
echo "==> Generating Radicle Swift bindings (UniFFI library mode)"
UNIFFI_OUT="$ROOT/target/radicle-uniffi"
rm -rf "$UNIFFI_OUT"
( cd "$ROOT" && cargo run --manifest-path bindgen/Cargo.toml --release -- \
    generate --library "$ROOT/target/$DEVICE/release/$LIBNAME" \
    --language swift --out-dir "$UNIFFI_OUT" )
for f in libradicle_uniffiFFI.h libradicle_uniffiFFI.modulemap libradicle_uniffi.swift; do
  [ -f "$UNIFFI_OUT/$f" ] || { echo "uniffi-bindgen did not produce $f" >&2; exit 1; }
done

# The scaffolding symbols are pulled in via a glob re-export, not a call
# site, so verify they actually survived into the archive. Capture-then-
# test instead of `nm | grep -q` (SIGPIPE race — see build-android.sh).
echo "==> Verifying Radicle scaffolding symbols in the device slice"
count="$(nm "$ROOT/target/$DEVICE/release/$LIBNAME" 2>/dev/null | grep -c 'uniffi_libradicle_uniffi_fn_func_start' || true)"
if [ "${count:-0}" -eq 0 ]; then
  echo "uniffi_libradicle_uniffi_fn_func_start missing from $LIBNAME" >&2
  exit 1
fi

# Resolve the C headers from whatever source cargo actually built — the
# pinned git tag (release) or a local path-dep checkout (local-dev) — so
# the staged headers can never drift from the compiled symbols. Each
# dep's `manifest_path` points at its `Cargo.toml`; `ant.h` lives in the
# crate's `include/`, `freedom_ipfs.h` at the freedom-ipfs repo root
# under `ffi/include/` (two levels up from the mobile crate).
echo "==> Resolving header sources from cargo metadata"
META="$(cd "$ROOT" && cargo metadata --format-version 1)"
ant_manifest="$(jq -r '.packages[] | select(.name=="ant-ffi") | .manifest_path' <<<"$META")"
ipfs_manifest="$(jq -r '.packages[] | select(.name=="freedom-ipfs-mobile") | .manifest_path' <<<"$META")"
myotis_manifest="$(jq -r '.packages[] | select(.name=="myotis-engine") | .manifest_path' <<<"$META")"
ANT_HEADER="$(dirname "$ant_manifest")/include/ant.h"
IPFS_HEADER="$(dirname "$ipfs_manifest")/../../ffi/include/freedom_ipfs.h"
# myotis_engine.h lives at the myotis Rust workspace root under include/
# (one level up from the myotis-engine crate).
MYOTIS_HEADER="$(dirname "$myotis_manifest")/../include/myotis_engine.h"
for h in "$ANT_HEADER" "$IPFS_HEADER" "$MYOTIS_HEADER"; do
  [ -f "$h" ] || { echo "header not found: $h" >&2; exit 1; }
done
echo "    ant.h           <- $ANT_HEADER"
echo "    freedom_ipfs.h  <- $IPFS_HEADER"
echo "    myotis_engine.h <- $MYOTIS_HEADER"

echo "==> Staging union headers + modulemap"
rm -rf "$OUT"
mkdir -p "$HDRS"
cp "$ANT_HEADER" "$HDRS/ant.h"
cp "$IPFS_HEADER" "$HDRS/freedom_ipfs.h"
cp "$MYOTIS_HEADER" "$HDRS/myotis_engine.h"
cp "$UNIFFI_OUT/libradicle_uniffiFFI.h" "$HDRS/libradicle_uniffiFFI.h"
# One modulemap file, two modules: the hand-written FreedomMobile union
# (C ABIs) plus the generated libradicle_uniffiFFI module the generated
# Swift imports. Concatenated at stage time so include/module.modulemap
# stays the single hand-maintained source.
cat "$ROOT/include/module.modulemap" "$UNIFFI_OUT/libradicle_uniffiFFI.modulemap" \
  > "$HDRS/module.modulemap"

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
echo "    Generated Swift API (copy into the app's RadicleKit target,"
echo "    Sources/RadicleKit/Generated/): $UNIFFI_OUT/libradicle_uniffi.swift"
