# Shared version readers and paths for check-version.sh, check-versions.sh and set-versions.sh.
# Sourced, not run. Nothing here changes the working directory: every path is absolute, built from
# this file's own location, so the scripts work from any directory.

_versions_lib_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$_versions_lib_dir/.." && pwd)"

# Where each version lives. Keep these paths in step with the manifests.
ROOT_MANIFEST="$REPO_ROOT/Cargo.toml"                 # [workspace.package] version, the default for every crate
SERVER_MANIFEST="$REPO_ROOT/crates/distlock-server/Cargo.toml"
CLIENT_MANIFEST="$REPO_ROOT/crates/distlock-client/Cargo.toml"
CORE_MANIFEST="$REPO_ROOT/crates/distlock-core/Cargo.toml"
PROTO_MANIFEST="$REPO_ROOT/crates/distlock-proto/Cargo.toml"
PY_CARGO="$REPO_ROOT/python/Cargo.toml"               # the PyO3 crate, `distlock-py`
PY_PROJECT="$REPO_ROOT/python/pyproject.toml"         # the `pydistlock` package
EXPECTED_FILE="$REPO_ROOT/crates/distlock-client/src/version.rs"
# Every Cargo.toml that may pin an inter-crate version: all crates plus the Python crate.
PIN_MANIFESTS=("$REPO_ROOT"/crates/*/Cargo.toml "$PY_CARGO")

# A path relative to the repository root, for messages.
rel() { printf '%s' "${1#"$REPO_ROOT"/}"; }

# The package's own version: the first `version = "..."` line (the package table comes first).
own_version() { sed -n 's/^version = "\(.*\)"$/\1/p' "$1" | head -n 1; }

root_version() { own_version "$ROOT_MANIFEST"; }

# A crate's version. A manifest that says `version.workspace = true` takes the root version.
manifest_version() {
  if grep -q '^version\.workspace = true$' "$1"; then root_version; else own_version "$1"; fi
}

# The version a dependency line pins, e.g. `distlock-core = { version = "0.3.0", path = ... }`.
pinned_version() { sed -n "s/^$2 = {.*version = \"\([^\"]*\)\".*/\1/p" "$1" | head -n 1; }

# The oldest server release the client accepts by default (`EXPECTED_SERVER_VERSION`).
expected_version() {
  sed -n 's/^pub(crate) const EXPECTED_SERVER_VERSION: &str = "\(.*\)";$/\1/p' "$EXPECTED_FILE" | head -n 1
}

# True when $1 is a release version, MAJOR.MINOR.PATCH.
is_release_version() { printf '%s\n' "$1" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$'; }

# True when $1 <= $2, compared as dotted numbers (0.9.9 < 0.10.0).
version_le() { [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n 1)" = "$1" ]; }
