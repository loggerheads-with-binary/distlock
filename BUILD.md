# Building and releasing distlock

Four Rust crates (one workspace) and one Python package (`python/`, its own build).

| Piece | What it is | Published to |
|---|---|---|
| `crates/distlock-proto` | protobuf wire types (the `.proto` lives in `crates/distlock-proto/proto/`) | crates.io, docs on docs.rs |
| `crates/distlock-core` | the lock-table state machine | crates.io, docs.rs |
| `crates/distlock-client` | async + blocking Rust client | crates.io, docs.rs |
| `crates/distlock-server` | the server binary (`distlock-server`) | crates.io (`cargo install`), or release binaries |
| `python/` (`pydistlock`, imports as `distlock`) | PyO3 wheel over the Rust client | PyPI |

## Prerequisites

* Rust 1.85 or newer (edition 2024). No `protoc` needed: `distlock-proto` compiles the `.proto` with `protox`.
* For the Python package: Python 3.9+ and [maturin](https://www.maturin.rs) (`pip install maturin`).
* For the tests only: `htpasswd` (Python tests make bcrypt hashes with `scripts/hash-password.sh`).

## Build and test

```bash
cargo build --release -p distlock-server      # target/release/distlock-server
cargo build --release -p distlock-client      # the Rust client library
cargo test --workspace                        # Rust tests (starts real servers on local ports)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Run the server: copy `.env.example` to `.env`, point `DISTLOCK_USERS_FILE` at a users file
(see `config/users.example.json`, and `scripts/hash-password.sh` for hashes), then run the binary.

Python (from `python/`):

```bash
python -m venv .venv && source .venv/bin/activate
pip install maturin pytest "pydantic>=2.5"
maturin develop                         # build the extension, install editable
(cd .. && cargo build -p distlock-server)   # the tests launch this binary
pytest
```

Tip: if your checkout is on an external or network volume, set `CARGO_TARGET_DIR` to a local
directory; builds there can fail with spurious errors.

## GitHub Actions

* `.github/workflows/ci.yml` runs on every pull request into `main` (and in the merge queue, and on
  pushes to `main`): `lint` (fmt, clippy, docs with warnings denied), `rust` (tests on Linux and macOS)
  and `python` (3.11 and 3.13, Linux and macOS). To make it gate merging, turn on branch protection
  for `main` (Settings > Branches) and require the status checks `lint`, `rust (...)` and `python (...)`.
  A workflow cannot block a merge by itself; the protection rule is what does.
* `.github/workflows/release.yml` runs when a tag `v*` is pushed. It checks that the tagged commit is
  on `main` and that every version in the repo equals the tag (`scripts/check-version.sh`), reruns the
  tests, builds the sdist and wheels (Linux x86_64/aarch64, macOS x86_64/arm64, Windows x64), and only
  if all of that succeeds publishes to crates.io and then PyPI. Re-running it after a partial failure is
  safe: versions already published are skipped.

  One-time setup: a repository secret `CARGO_REGISTRY_TOKEN`; a PyPI
  [trusted publisher](https://docs.pypi.org/trusted-publishers/) for this repo (workflow `release.yml`,
  environment `pypi`); and GitHub environments `crates-io` and `pypi` (add required reviewers there
  for a manual approval step before publishing).

  To release: bump the versions (step 0 below), merge to `main`, then
  `git tag v0.2.0 && git push origin v0.2.0`.

The manual steps below are the same thing done by hand, and the fallback if the workflow is unavailable.

## Releasing

### 0. Prepare

1. Bump the version in **all** of: `[workspace.package]` in `Cargo.toml`, `python/Cargo.toml`,
   `python/pyproject.toml`, and the `version = "..."` on the inter-crate dependencies
   (`distlock-proto`, `distlock-core` in the crates that use them).
2. Run the full build and test section above, plus the docs check:
   `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`
3. Check them with `scripts/check-version.sh v0.2.0`, commit, merge to main, then tag: `git tag v0.2.0`.

### 1. crates.io (and docs.rs)

Log in once with `cargo login` (a crates.io API token). Publish in dependency order, and
dry-run each first:

```bash
cargo publish --dry-run -p distlock-proto && cargo publish -p distlock-proto
cargo publish -p distlock-core
cargo publish -p distlock-client
cargo publish -p distlock-server
```

* A dry run of `distlock-client` and `distlock-server` only succeeds once `distlock-proto`
  (and `distlock-core`) are live on crates.io, because the packaged crate is built against the
  registry rather than your path dependencies. If it says a dependency is not found right after
  you published it, wait a minute and retry.
* Publishing is permanent: a version can be yanked but never replaced or deleted.
* **docs.rs builds automatically** after each publish from the crate's own docs
  (the `//!` comments in `src/lib.rs`), at `https://docs.rs/distlock-client` and so on. Nothing
  to configure: the build needs no system `protoc`. Check the result is green on the crate's
  docs.rs "builds" page. The `RUSTDOCFLAGS="-D warnings" cargo doc` check above catches broken
  doc links before you publish.
* Users install the server with `cargo install distlock-server`, and use the client with
  `cargo add distlock-client`.

### 2. PyPI

The wheel is `abi3`, so one wheel per OS and CPU architecture serves every Python from 3.9 up.
Build and upload an sdist plus the wheels:

```bash
cd python
maturin sdist -o dist                    # source distribution (bundles the Rust crates it needs)
maturin build --release -o dist          # a wheel for THIS machine's OS and architecture
twine check dist/*
twine upload dist/*                      # a PyPI API token; use `--repository testpypi` to rehearse
```

* The sdist contains `crates/distlock-client` and `crates/distlock-proto`, so
  `pip install pydistlock` from source works on a machine with a Rust toolchain, without
  crates.io having your crates. Publishing to PyPI does not depend on step 1.
* A wheel is built only for the machine you run it on. For Linux (manylinux) and Windows wheels,
  build in CI, for example with `PyO3/maturin-action` on a matrix of runners
  (`ubuntu`, `macos`, `windows`, with `--manylinux auto` on Linux), collect all wheels in
  `dist/`, then upload them together. PyPI "trusted publishing" avoids storing a token.
* Rehearse first: upload to TestPyPI, then
  `pip install -i https://test.pypi.org/simple/ --extra-index-url https://pypi.org/simple/ pydistlock`
  (the extra index is for `pydantic`).
* As with crates.io, a PyPI version can never be re-uploaded; bump the version for any fix.

### What was checked, and what was not

Checked on macOS (arm64): `cargo publish --dry-run` for `distlock-proto` and `distlock-core`,
warning-free `cargo doc`, and a wheel built from the sdist that passes `twine check`.
Not checked: an actual upload to either registry, a docs.rs build, Linux/Windows wheels,
and the Rust minimum version (the 1.85 floor is what edition 2024 requires; no older toolchain
was tried).
