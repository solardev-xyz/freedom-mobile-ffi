#!/usr/bin/env bash
# Refuse to release against unreleased node pins.
#
# Every git dependency in Cargo.toml must be pinned by `tag = "..."`. A
# `rev`/`branch` pin (or none at all) points at an upstream PR head that
# can vanish once that PR is squash-merged and its branch deleted, after
# which a clean `cargo fetch` of the released tag no longer resolves.
# Uncommented `path = ...` deps are local-dev only and refused as well.
#
# Usage: scripts/check-release-pins.sh [Cargo.toml]
# Exit 0 if every pin is a tag, 1 otherwise (one line per offender).
set -euo pipefail

MANIFEST="${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/Cargo.toml}"

bad=0
while IFS= read -r line; do
  name="${line%%=*}"; name="${name// /}"
  if [[ "$line" =~ git[[:space:]]*= ]]; then
    if [[ ! "$line" =~ tag[[:space:]]*=[[:space:]]*\" ]] \
       || [[ "$line" =~ (rev|branch)[[:space:]]*= ]]; then
      echo "unreleased pin: $name is a git dep not pinned by tag: $line" >&2
      bad=1
    fi
  elif [[ "$line" =~ path[[:space:]]*=[[:space:]]*\"\.\./ ]]; then
    echo "unreleased pin: $name is a local path dep: $line" >&2
    bad=1
  fi
done < <(grep -vE '^[[:space:]]*#' "$MANIFEST" | grep -E '\{.*(git|path)[[:space:]]*=' || true)

if [ "$bad" -ne 0 ]; then
  echo "Swap the offending deps for release tags before cutting a release." >&2
  exit 1
fi
echo "All git deps in $MANIFEST are pinned by tag."
