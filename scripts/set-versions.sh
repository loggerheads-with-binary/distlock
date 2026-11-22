#!/usr/bin/env bash
# Set the version of the server, the client, the Python package and the client's expected server version.
#
#   scripts/set-versions.sh --version 0.4.0                 # all four to 0.4.0
#   scripts/set-versions.sh --version 0.4.0 --client 0.4.1  # all four to 0.4.0, but the client to 0.4.1
#   scripts/set-versions.sh --server 0.4.0 --python 0.4.0   # only those two (and the pins that name them)
#   scripts/set-versions.sh --expected 0.3.0 --dry-run      # show what would change, change nothing
#
# --version is the common value: each of server, client, python and expected takes it unless its own flag is given.
# The workspace version (Cargo.toml) takes --version when it is given, and core and proto follow it.
# A crate whose version equals the workspace version inherits it (`version.workspace = true`), as before.
# Runs from any directory. Full guide: scripts/README.md.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/versions-lib.sh"

usage() {
  cat >&2 <<'EOF'
usage: set-versions.sh [--version X.Y.Z] [--server X.Y.Z] [--client X.Y.Z] [--python X.Y.Z] [--expected X.Y.Z] [--dry-run]
  --version   common version for all four below (unless a specific flag is given)
  --server    distlock-server crate
  --client    distlock-client crate (and the version pinned by distlock-py)
  --python    pydistlock package: python/pyproject.toml and python/Cargo.toml (distlock-py)
  --expected  EXPECTED_SERVER_VERSION in crates/distlock-client/src/version.rs
  --dry-run   print the changes without making them
EOF
  exit 2
}

common="" server="" client="" python="" expected="" dry=0
while [ $# -gt 0 ]; do
  case "$1" in
    --version)  common="${2:-}";   shift 2 ;;
    --server)   server="${2:-}";   shift 2 ;;
    --client)   client="${2:-}";   shift 2 ;;
    --python)   python="${2:-}";   shift 2 ;;
    --expected) expected="${2:-}"; shift 2 ;;
    --dry-run)  dry=1; shift ;;
    -h|--help)  usage ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done
if [ -z "$common$server$client$python$expected" ]; then usage; fi
for v in "$common" "$server" "$client" "$python" "$expected"; do
  if [ -n "$v" ] && ! is_release_version "$v"; then
    echo "'$v' is not a version like 1.2.3" >&2; exit 2
  fi
done

# The new value of each version. Empty means "keep what is there now".
root_now="$(root_version)"
server_now="$(manifest_version "$SERVER_MANIFEST")"
client_now="$(manifest_version "$CLIENT_MANIFEST")"
python_now="$(manifest_version "$PY_PROJECT")"
expected_now="$(expected_version)"

root_new="${common:-$root_now}"
server_new="${server:-${common:-$server_now}}"
client_new="${client:-${common:-$client_now}}"
python_new="${python:-${common:-$python_now}}"
expected_new="${expected:-${common:-$expected_now}}"

# Run a sed edit on one file, or describe it under --dry-run.
edit() { # edit FILE SCRIPT WHAT
  if [ "$dry" = 1 ]; then echo "would set $3 in $(rel "$1")"; return; fi
  sed -i.bak -e "$2" "$1" && rm -f "$1.bak"
  echo "set $3 in $(rel "$1")"
}

# Set a crate's own version. Equal to the workspace version: inherit it. Otherwise write it out.
set_crate_version() { # set_crate_version MANIFEST VERSION
  local line
  if [ "$2" = "$root_new" ]; then line='version.workspace = true'; else line="version = \"$2\""; fi
  edit "$1" "s#^version.*\$#$line#" "version $2"
}

# Pins: `dep = { version = "X", path = ... }` in every crate and in the Python crate.
set_pins() { # set_pins DEP VERSION
  for f in "${PIN_MANIFESTS[@]}"; do
    grep -q "^$1 = {" "$f" || continue
    edit "$f" "s|^\($1 = {.*version = \"\)[^\"]*\"|\1$2\"|" "pin $1 = $2"
  done
}

if [ "$root_new" != "$root_now" ]; then
  edit "$ROOT_MANIFEST" "s|^version = \".*\"\$|version = \"$root_new\"|" "workspace version $root_new"
fi
set_crate_version "$SERVER_MANIFEST" "$server_new"
set_crate_version "$CLIENT_MANIFEST" "$client_new"
edit "$PY_PROJECT" "s|^version = \".*\"\$|version = \"$python_new\"|" "python package $python_new"
edit "$PY_CARGO" "s|^version = \".*\"\$|version = \"$python_new\"|" "distlock-py $python_new"
set_pins distlock-proto "$root_new"
set_pins distlock-core "$root_new"
set_pins distlock-client "$client_new"
edit "$EXPECTED_FILE" "s|^\(pub(crate) const EXPECTED_SERVER_VERSION: &str = \"\)[^\"]*\"|\1$expected_new\"|" "expected server $expected_new"

if [ "$dry" = 0 ] && command -v cargo >/dev/null 2>&1; then
  # Refresh only the workspace packages' entries in Cargo.lock. External dependencies stay as locked.
  cargo update --workspace --quiet --manifest-path "$ROOT_MANIFEST"
  echo "refreshed workspace entries in $(rel "$REPO_ROOT/Cargo.lock")"
elif [ "$dry" = 0 ]; then
  echo "cargo not found: Cargo.lock not refreshed; run cargo update --workspace" >&2
fi
if [ "$dry" = 0 ]; then
  echo "review with: $SCRIPT_DIR/check-versions.sh"
fi
