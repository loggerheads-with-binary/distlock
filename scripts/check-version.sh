#!/usr/bin/env bash
# Check that a release tag (v1.2.3) matches every place the version is written.
#   scripts/check-version.sh v1.2.3
set -euo pipefail
cd "$(dirname "$0")/.."

tag="${1:?usage: check-version.sh vX.Y.Z}"
case "$tag" in v[0-9]*) ;; *) echo "tag '$tag' is not of the form vX.Y.Z" >&2; exit 2 ;; esac
want="${tag#v}"

# First `version = "..."` line in a file (the package's own version comes first in each manifest).
first_version() { sed -n 's/^version = "\(.*\)"$/\1/p' "$1" | head -n 1; }

fail=0
check() {
  if [ "$2" != "$want" ]; then echo "MISMATCH: $1 is '$2', tag says '$want'" >&2; fail=1; fi
}
check "Cargo.toml [workspace.package]" "$(first_version Cargo.toml)"
check "python/Cargo.toml" "$(first_version python/Cargo.toml)"
check "python/pyproject.toml" "$(first_version python/pyproject.toml)"

# Inter-crate dependencies pin an exact-enough version, which must be satisfied by this release.
for f in crates/*/Cargo.toml python/Cargo.toml; do
  for dep in distlock-proto distlock-core distlock-client; do
    pinned="$(sed -n "s/^$dep = {.*version = \"\([^\"]*\)\".*/\1/p" "$f")"
    if [ -n "$pinned" ] && [ "$pinned" != "$want" ]; then
      echo "MISMATCH: $f depends on $dep $pinned, tag says '$want'" >&2; fail=1
    fi
  done
done
[ "$fail" = 0 ] && echo "all versions match $want"
exit "$fail"
