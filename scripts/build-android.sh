#!/usr/bin/env bash
# Build libfreedom_mobile_ffi.so: the combined Swarm (ant-ffi) + IPFS
# (freedom-ipfs-mobile) Rust cdylib for Android, one .so per ABI, with
# the union C header surface staged alongside.
#
# ABIs: arm64-v8a (aarch64-linux-android) and x86_64 (emulator).
# Toolchain comes from rust-toolchain.toml plus cargo-ndk and the
# Android NDK (ANDROID_NDK_HOME or cargo-ndk's own discovery).
#
# Notes mirrored from ant's `cargo xtask build-android-*`:
#   - API level 26 (`-P`), matching the node repos' Android floor.
#   - The inner `cargo rustc --crate-type cdylib` overrides the [lib]
#     crate-type list so only the .so is emitted; staticlib + rlib stay
#     reserved for the iOS/xcframework slices.
#   - `--no-default-features` drops the `chain` gateway surfaces (see
#     Cargo.toml); freedom-browser-android has no wallet UI.
#   - cargo emits cdylibs without a SONAME, and an absolute path in
#     DT_NEEDED breaks dlopen on-device — set one explicitly so
#     consumers can link the .so directly instead of by name+dir.
set -euo pipefail

CRATE=freedom-mobile-ffi
LIBNAME=libfreedom_mobile_ffi.so
PROFILE=release-android
API_LEVEL=26
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/target/android"
HDRS="$OUT/headers"

# rust target triple -> Gradle jniLibs ABI directory name
declare -A ABI=(
  [aarch64-linux-android]=arm64-v8a
  [x86_64-linux-android]=x86_64
)

command -v cargo-ndk >/dev/null || { echo "cargo-ndk not installed (cargo install cargo-ndk)" >&2; exit 1; }

echo "==> Building $PROFILE cdylibs"
for t in "${!ABI[@]}"; do
  echo "    - $t"
  ( cd "$ROOT" && cargo ndk -t "$t" -P "$API_LEVEL" -- rustc -p "$CRATE" \
      --profile "$PROFILE" --no-default-features --crate-type cdylib \
      -- -C link-arg=-Wl,-soname,"$LIBNAME" )
done

# Resolve the C headers from whatever source cargo actually built — the
# pinned git tag (release) or a local path-dep checkout (local-dev) — so
# the staged headers can never drift from the compiled symbols. Same
# layout logic as build-xcframework.sh.
echo "==> Resolving header sources from cargo metadata"
META="$(cd "$ROOT" && cargo metadata --format-version 1)"
ant_manifest="$(jq -r '.packages[] | select(.name=="ant-ffi") | .manifest_path' <<<"$META")"
ipfs_manifest="$(jq -r '.packages[] | select(.name=="freedom-ipfs-mobile") | .manifest_path' <<<"$META")"
ANT_HEADER="$(dirname "$ant_manifest")/include/ant.h"
IPFS_HEADER="$(dirname "$ipfs_manifest")/../../ffi/include/freedom_ipfs.h"
for h in "$ANT_HEADER" "$IPFS_HEADER"; do
  [ -f "$h" ] || { echo "header not found: $h" >&2; exit 1; }
done
echo "    ant.h          <- $ANT_HEADER"
echo "    freedom_ipfs.h <- $IPFS_HEADER"

echo "==> Staging jniLibs tree + headers"
rm -rf "$OUT"
mkdir -p "$HDRS"
cp "$ANT_HEADER" "$HDRS/ant.h"
cp "$IPFS_HEADER" "$HDRS/freedom_ipfs.h"
for t in "${!ABI[@]}"; do
  mkdir -p "$OUT/jniLibs/${ABI[$t]}"
  cp "$ROOT/target/$t/$PROFILE/$LIBNAME" "$OUT/jniLibs/${ABI[$t]}/$LIBNAME"
done

# Guard against LTO/linker stripping regressions: both C surfaces must
# be dynamic-exported, and the SONAME must have stuck. Prefer the NDK's
# llvm tools (always present when the build itself succeeded, and
# guaranteed to read Android ELFs); fall back to host tools for odd
# setups where ANDROID_NDK_HOME isn't exported but cargo-ndk found the
# NDK its own way.
echo "==> Verifying exported symbols + SONAME"
NDK_BIN="$(ls -d "${ANDROID_NDK_HOME:-/nonexistent}"/toolchains/llvm/prebuilt/*/bin 2>/dev/null | head -1)"
NM="${NDK_BIN:+$NDK_BIN/llvm-nm}"; NM="${NM:-$(command -v llvm-nm || echo nm)}"
READELF="${NDK_BIN:+$NDK_BIN/llvm-readelf}"; READELF="${READELF:-$(command -v llvm-readelf || echo readelf)}"
fail_with_diagnostics() {
  local so="$1" why="$2"
  {
    echo "$so: $why"
    echo "--- tools: NM=$NM READELF=$READELF"
    file "$so" 2>/dev/null || true
    echo "--- ELF header + dynamic:"
    "$READELF" -h -d "$so" 2>&1 | head -30
    echo "--- dynamic T-symbol count: $("$NM" -D "$so" 2>/dev/null | grep -c ' T ' || true)"
    echo "--- first dynamic symbols:"
    "$NM" -D "$so" 2>&1 | head -25
  } >&2
  exit 1
}

# Capture tool output before grepping: under `set -o pipefail`,
# `nm | grep -q` is a latent race — grep -q exits on first match,
# nm dies of SIGPIPE mid-write, and the pipeline "fails" even though
# the symbol is present. (Exactly this bit the first CI runs.)
for t in "${!ABI[@]}"; do
  so="$OUT/jniLibs/${ABI[$t]}/$LIBNAME"
  dynsyms="$("$NM" -D "$so")"
  for sym in ant_init ant_start_gateway freedom_ipfs_node_new_with_data_dir freedom_ipfs_node_start_gateway_online; do
    grep -q " T $sym" <<<"$dynsyms" || fail_with_diagnostics "$so" "missing export $sym"
  done
  dyn="$("$READELF" -d "$so")"
  grep -q "SONAME.*$LIBNAME" <<<"$dyn" || fail_with_diagnostics "$so" "missing SONAME"
  echo "    ${ABI[$t]}: $(du -h "$so" | cut -f1) ok"
done

echo "==> Done: $OUT/jniLibs"
