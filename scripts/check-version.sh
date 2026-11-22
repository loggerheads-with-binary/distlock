#!/usr/bin/env bash
# Check that a release tag (v1.2.3) matches every place the version is written.
#   scripts/check-version.sh v1.2.3
# The client's expected server version may be lower than the tag (older servers still work), but not higher.
# Runs from any directory. Used by .github/workflows/release.yml.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/versions-lib.sh"
tag="${1:?usage: check-version.sh vX.Y.Z}"
case "$tag" in v[0-9]*) ;; *) echo "tag '$tag' is not of the form vX.Y.Z" >&2; exit 2 ;; esac
want="${tag#v}"
fail=0
check() {
  if [ "$2" != "$want" ]; then echo "MISMATCH: $1 is '$2', tag says '$want'" >&2; fail=1; fi
}
check "Cargo.toml [workspace.package]" "$(root_version)"
check "$(rel "$SERVER_MANIFEST")" "$(manifest_version "$SERVER_MANIFEST")"
check "$(rel "$CLIENT_MANIFEST")" "$(manifest_version "$CLIENT_MANIFEST")"
check "$(rel "$CORE_MANIFEST")" "$(manifest_version "$CORE_MANIFEST")"
check "$(rel "$PROTO_MANIFEST")" "$(manifest_version "$PROTO_MANIFEST")"
check "$(rel "$PY_CARGO")" "$(manifest_version "$PY_CARGO")"
check "$(rel "$PY_PROJECT")" "$(manifest_version "$PY_PROJECT")"
# The client accepts servers at or above this version by default. It may be lower than the release.
expected="$(expected_version)"
if [ -z "$expected" ]; then
  echo "MISMATCH: $(rel "$EXPECTED_FILE") has no EXPECTED_SERVER_VERSION" >&2; fail=1
elif ! version_le "$expected" "$want"; then
  echo "MISMATCH: $(rel "$EXPECTED_FILE") expects server '$expected', which is higher than '$want'" >&2; fail=1
fi
# Inter-crate dependencies pin a version, which must be satisfied by this release.
for f in "${PIN_MANIFESTS[@]}"; do
  for dep in distlock-proto distlock-core distlock-client; do
    pinned="$(pinned_version "$f" "$dep")"
    if [ -n "$pinned" ] && [ "$pinned" != "$want" ]; then
      echo "MISMATCH: $(rel "$f") depends on $dep $pinned, tag says '$want'" >&2; fail=1
    fi
  done
done
[ "$fail" = 0 ] && echo "all versions match $want (expected server version $expected may be lower)"
exit "$fail"
