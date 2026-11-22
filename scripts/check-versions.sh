#!/usr/bin/env bash
# Print the version of every package and flag the ones that differ from the baseline.
#   scripts/check-versions.sh                 # baseline: the version in the top-level Cargo.toml
#   scripts/check-versions.sh --baseline 0.4.0
# Red: differs from the baseline. Yellow: the expected server version is lower than the baseline, which is allowed.
# Exit 1 if anything is red, 2 on bad arguments. Colour is off when stdout is not a terminal or NO_COLOR is set.
# Runs from any directory.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/versions-lib.sh"

usage() { echo "usage: check-versions.sh [--baseline X.Y.Z]" >&2; exit 2; }
baseline=""
while [ $# -gt 0 ]; do
  case "$1" in
    --baseline) baseline="${2:-}"; shift 2 ;;
    -h|--help) usage ;;
    *) usage ;;
  esac
done
[ -z "$baseline" ] && baseline="$(root_version)"
if ! is_release_version "$baseline"; then
  echo "baseline '$baseline' is not a version like 1.2.3" >&2; exit 2
fi

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  RED=$'\033[31m'; YELLOW=$'\033[33m'; GREEN=$'\033[32m'; RESET=$'\033[0m'
else
  RED=""; YELLOW=""; GREEN=""; RESET=""
fi

red=0
row() { # row NAME VERSION SOURCE STATUS(ok|lower|red)
  local colour="$GREEN" note=""
  case "$4" in
    red) colour="$RED"; red=1; note="  <- differs from $baseline" ;;
    lower) colour="$YELLOW"; note="  <- lower than $baseline (allowed)" ;;
  esac
  printf '%s%-18s %-10s %s%s%s\n' "$colour" "$1" "$2" "$3" "$note" "$RESET"
}
# A crate or package row: green when it matches the baseline, red when not.
pkg() { # pkg NAME MANIFEST
  local v; v="$(manifest_version "$2")"
  if [ "$v" = "$baseline" ]; then row "$1" "$v" "$(rel "$2")" ok; else row "$1" "$v" "$(rel "$2")" red; fi
}

printf '%-18s %-10s %s\n' "PACKAGE" "VERSION" "SOURCE"
row "workspace (base)" "$baseline" "$(rel "$ROOT_MANIFEST")" ok
pkg "distlock-core" "$CORE_MANIFEST"
pkg "distlock-proto" "$PROTO_MANIFEST"
pkg "distlock-client" "$CLIENT_MANIFEST"
pkg "distlock-server" "$SERVER_MANIFEST"
pkg "distlock-py" "$PY_CARGO"
pkg "pydistlock" "$PY_PROJECT"
expected="$(expected_version)"
if [ -z "$expected" ]; then
  row "expected server" "(none)" "$(rel "$EXPECTED_FILE")" red
elif [ "$expected" = "$baseline" ]; then
  row "expected server" "$expected" "$(rel "$EXPECTED_FILE")" ok
elif version_le "$expected" "$baseline"; then
  row "expected server" "$expected" "$(rel "$EXPECTED_FILE")" lower
else
  row "expected server" "$expected" "$(rel "$EXPECTED_FILE")" red
fi

[ "$red" = 0 ] || { echo "some versions differ from $baseline" >&2; exit 1; }
