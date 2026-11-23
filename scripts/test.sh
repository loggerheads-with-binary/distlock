#!/usr/bin/env bash
# Run the Rust and Python tests the way CI does.
#
#   scripts/test.sh            # both
#   scripts/test.sh rust       # Rust only
#   scripts/test.sh python     # Python only (extra args go to pytest, which runs under pytest-xdist: scripts/test.sh python -k lock)
#
# Runs from any directory: every path is absolute, built from this script's own location.
#
# Needs: a Rust toolchain, cargo-nextest (`cargo install cargo-nextest --locked`), Python 3.9+, and `htpasswd` (macOS has it; `apache2-utils` on Linux).
# The Python tests start the real server, so this builds it first. Uses its own venv, ~/.cache/distlock-test-venv (override with DISTLOCK_TEST_VENV); python/.venv is left
# alone. The venv is kept off the repo because creating one fails on some external volumes.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$SCRIPT_DIR/.." && pwd)"
what="${1:-all}"
[ "$#" -gt 0 ] && shift
case "$what" in all | rust | python) ;; *) echo "usage: $0 [all|rust|python] [pytest args]" >&2; exit 2 ;; esac
command -v htpasswd >/dev/null || { echo "htpasswd not found (install apache2-utils)" >&2; exit 1; }
run_rust() {
  command -v cargo-nextest >/dev/null || { echo "cargo-nextest not found (cargo install cargo-nextest --locked)" >&2; exit 1; }
  echo "==> cargo nextest run --workspace"
  cargo nextest run --manifest-path "$root/Cargo.toml" --workspace --locked
  # nextest does not run doctests.
  echo "==> cargo test --doc --workspace"
  cargo test --manifest-path "$root/Cargo.toml" --workspace --doc --locked
}
run_python() {
  echo "==> building the server the Python tests start"
  cargo build --manifest-path "$root/Cargo.toml" -p distlock-server --locked
  # The tests look in ../target/debug unless told otherwise; honour a custom target dir.
  export DISTLOCK_SERVER_BIN="${DISTLOCK_SERVER_BIN:-${CARGO_TARGET_DIR:-$root/target}/debug/distlock-server}"
  venv="${DISTLOCK_TEST_VENV:-${XDG_CACHE_HOME:-$HOME/.cache}/distlock-test-venv}"
  [ -f "$venv/bin/activate" ] || { rm -rf "$venv"; python3 -m venv "$venv"; }
  . "$venv/bin/activate"
  pip install --quiet maturin pytest pytest-xdist "pydantic>=2.5"
  echo "==> maturin develop (python/)"
  maturin develop --quiet --manifest-path "$root/python/Cargo.toml"
  echo "==> pytest (python/)"
  # Config and rootdir come from python/pyproject.toml. The test folder is given explicitly: pytest only
  # uses the config's testpaths when the current directory is the rootdir.
  # pytest-xdist runs the tests on parallel workers (one per CPU); pass `-n 0` to run in one process.
  pytest -c "$root/python/pyproject.toml" --rootdir "$root/python" "$root/python/tests" -n auto "$@"
}
[ "$what" = python ] || run_rust
[ "$what" = rust ] || run_python "$@"
echo "==> all tests passed"
