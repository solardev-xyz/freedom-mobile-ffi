#!/usr/bin/env bash
# Refuse to release against unreleased node pins.
#
# Every git dependency in Cargo.toml must be pinned by `tag = "..."`. A
# `rev`/`branch` pin (or none at all) points at an upstream PR head that
# can vanish once that PR is squash-merged and its branch deleted, after
# which a clean `cargo fetch` of the released tag no longer resolves.
# `path = ...` deps that leave this repo (sibling checkouts, absolute
# paths) are local-dev only and refused as well; in-repo paths such as
# `vendor/` ship with the tag and are fine.
#
# The manifest is parsed as TOML (python3 >= 3.11 `tomllib`), so every
# spelling is covered: inline tables (one line or several) and
# `[dependencies.<name>]` tables, in [dependencies], [dev-dependencies],
# [build-dependencies], their [target.<cfg>.*] variants, and [patch.*].
# A manifest the parser can't read fails the check rather than passing.
#
# Usage: scripts/check-release-pins.sh [Cargo.toml]
# Exit 0 if every pin is a tag, 1 otherwise (one line per offender).
set -euo pipefail

MANIFEST="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/Cargo.toml}"

exec python3 - "$MANIFEST" <<'PY'
import os, sys
try:
    import tomllib
except ImportError:
    sys.exit("check-release-pins: needs python3 >= 3.11 (tomllib)")

manifest = os.path.abspath(sys.argv[1])
root = os.path.dirname(manifest)
try:
    with open(manifest, "rb") as f:
        doc = tomllib.load(f)
except tomllib.TOMLDecodeError as e:
    # Fail closed: e.g. a TOML 1.1 multi-line inline table this python
    # can't read must not slip through the gate unchecked.
    sys.exit(f"unreleased pin: cannot parse {manifest} ({e}); refusing to vouch for its pins")

KINDS = ("dependencies", "dev-dependencies", "build-dependencies")
tables = [(k, doc.get(k, {})) for k in KINDS]
for cfg, t in doc.get("target", {}).items():
    tables += [(f"target.{cfg}.{k}", t.get(k, {})) for k in KINDS]
for src, t in doc.get("patch", {}).items():
    tables.append((f"patch.{src}", t))

bad = []
for section, deps in tables:
    for name, spec in deps.items():
        if not isinstance(spec, dict):
            continue  # plain version string: a crates.io release
        where = f"[{section}] {name}"
        if "git" in spec:
            if "tag" not in spec or "rev" in spec or "branch" in spec:
                pin = {k: spec[k] for k in ("tag", "rev", "branch") if k in spec}
                bad.append(f"{where} is a git dep not pinned by tag: {pin or 'unpinned'}")
        elif "path" in spec:
            p = spec["path"]
            real = os.path.realpath(os.path.join(root, p))
            if os.path.isabs(p) or os.path.commonpath([real, os.path.realpath(root)]) != os.path.realpath(root):
                bad.append(f"{where} is a local path dep outside the repo: {p}")

for b in bad:
    print(f"unreleased pin: {b}", file=sys.stderr)
if bad:
    print("Swap the offending deps for release tags before cutting a release.", file=sys.stderr)
    sys.exit(1)
print(f"All git deps in {manifest} are pinned by tag.")
PY
