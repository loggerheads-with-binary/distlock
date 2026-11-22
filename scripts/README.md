# Scripts

Helper scripts for releasing, testing and running distlock. Each one can be run from any directory:
it finds the repository from its own location, and it does not change the directory you ran it from.
Paths you pass in (such as `--dir` or `--cert`) are relative to where you run the script, and
the example paths below are relative to the repository root.

| Script | What it does |
|---|---|
| [`set-versions.sh`](#set-versionssh) | Sets the server, client, Python and expected-server versions. |
| [`check-versions.sh`](#check-versionssh) | Lists every package version and flags any that differ. |
| [`check-version.sh`](#check-versionsh) | Checks that every version matches a release tag. CI runs it on tag push. |
| [`test.sh`](#testsh) | Runs the Rust and Python test suites the way CI does. |
| [`hash-password.sh`](#hash-passwordsh) | Makes the bcrypt hash for a user in `config/users.json`. |
| [`tls/`](#tls) | Makes a CA and server certificates, or uses your own CA. |
| `versions-lib.sh` | Shared code for the version scripts. Sourced, never run. |

## Where versions live

distlock has four version numbers. They are kept in step by hand, or with `set-versions.sh`.

| Version | Where it is written | Notes |
|---|---|---|
| **workspace** (base) | `Cargo.toml`, `[workspace.package]` | The default for every Rust crate. |
| **server** | `crates/distlock-server/Cargo.toml` | Either `version.workspace = true` or its own `version = "..."`. |
| **client** | `crates/distlock-client/Cargo.toml` | Same as the server. |
| **Python** | `python/pyproject.toml` (`pydistlock`) and `python/Cargo.toml` (`distlock-py`) | Both must match. `pydistlock.__version__` comes from `distlock-py`. |
| **expected server** | `EXPECTED_SERVER_VERSION` in `crates/distlock-client/src/version.rs` | The oldest server the client accepts by default (the `min_version` default). It may be **lower** than the release, because older servers still work with the client. It must not be higher. |

`core` and `proto` (`crates/distlock-core`, `crates/distlock-proto`) take the workspace version.
Inter-crate dependencies pin the version of the crate they use, for example
`distlock-core = { version = "0.3.0", path = ... }`. `set-versions.sh` keeps those pins in step.

## Release workflow

```bash
scripts/set-versions.sh --version 0.4.0 --dry-run    # preview
scripts/set-versions.sh --version 0.4.0              # apply
scripts/check-versions.sh                            # every package should be green
scripts/test.sh                                      # run the tests
scripts/check-version.sh v0.4.0                      # the tag check CI runs
git commit -am "Release 0.4.0" && git tag v0.4.0     # then push the tag
```

If the versions must differ on purpose, for example a client patch release on the same server,
use the specific flags (see `set-versions.sh` below) and expect `check-versions.sh` to show the
different package in red. `check-version.sh` will then fail for that tag, because it wants every
version to equal the tag. Either keep the versions equal, or change the check.

## `set-versions.sh`

Sets the four versions above.

```bash
scripts/set-versions.sh --version 0.4.0                  # all four to 0.4.0
scripts/set-versions.sh --version 0.4.0 --client 0.4.1   # all four to 0.4.0, but the client to 0.4.1
scripts/set-versions.sh --server 0.4.2                   # only the server
scripts/set-versions.sh --expected 0.3.0 --dry-run       # preview a change to the expected version
```

| Flag | Sets |
|---|---|
| `--version X.Y.Z` | The common value. Each of server, client, Python and expected takes it, unless its own flag is given. Also sets the workspace version. |
| `--server X.Y.Z` | `distlock-server` |
| `--client X.Y.Z` | `distlock-client`, and the pin of it in `distlock-py` |
| `--python X.Y.Z` | `pydistlock` and `distlock-py` |
| `--expected X.Y.Z` | `EXPECTED_SERVER_VERSION` |
| `--dry-run` | Prints each change, and changes nothing. |

What it does:

* A crate whose version equals the workspace version is written as `version.workspace = true`.
  A different version is written out as `version = "..."`.
* Every pin to `distlock-core`, `distlock-proto` or `distlock-client` is updated to the version of
  that crate.
* It refreshes the workspace entries in `Cargo.lock` with `cargo update --workspace`. External
  dependencies stay as locked. This needs no network access. Without `cargo` on the PATH, it warns
  and skips this step.
* It rejects any version that is not `MAJOR.MINOR.PATCH`, with exit status 2.
* It does not check that each edit worked. If a manifest changes format, an edit can silently do
  nothing. Run `check-versions.sh` afterwards.

## `check-versions.sh`

Lists every package version, compared with the workspace version in `Cargo.toml`.

```bash
scripts/check-versions.sh                  # baseline: the workspace version
scripts/check-versions.sh --baseline 0.4.0 # compare with another version
```

```
PACKAGE            VERSION    SOURCE
workspace (base)   0.3.0      Cargo.toml
distlock-core      0.3.0      crates/distlock-core/Cargo.toml
distlock-client    0.4.1      crates/distlock-client/Cargo.toml  <- differs from 0.3.0
expected server    0.3.0      crates/distlock-client/src/version.rs
```

* **Green**: matches the baseline.
* **Red**: differs from the baseline. The script then exits with status 1.
* **Yellow**: the expected server version is lower than the baseline. This is allowed, so it does
  not cause exit 1.
* Colour is used only when the output is a terminal. Set `NO_COLOR=1` to turn it off.
* Exit status: 0 when nothing is red, 1 when something is, 2 for a bad `--baseline`.

## `check-version.sh`

Checks that a release tag matches every version, and that the pins are satisfied.

```bash
scripts/check-version.sh v0.4.0
```

* Every package version must equal the tag, with one exception: the expected server version may be
  lower than the tag.
* It fails if the expected server version is higher than the tag.
* It checks the pins to `distlock-core`, `distlock-proto` and `distlock-client`.
* Exit status: 0 on success, 1 on any mismatch, 2 when the tag is not of the form `vX.Y.Z`.
* `.github/workflows/release.yml` runs it on every tag push.

## `test.sh`

Runs the test suites the way CI does.

```bash
scripts/test.sh             # Rust and Python
scripts/test.sh rust        # cargo test --workspace
scripts/test.sh python      # build the server, maturin develop, pytest
scripts/test.sh python -k only_waiters    # extra arguments go to pytest
```

* Needs a Rust toolchain, Python 3.9 or later, and `htpasswd` (preinstalled on macOS; the
  `apache2-utils` package on Linux).
* The Python part builds `distlock-server` first, and sets `DISTLOCK_SERVER_BIN` to that binary.
  It honours `CARGO_TARGET_DIR` for the binary location.
* It uses its own virtual environment at `~/.cache/distlock-test-venv`. Set `DISTLOCK_TEST_VENV` to
  use another location. `python/.venv` is not used.
* Pytest always collects `python/tests`. Select tests with options such as `-k` or `-m`, not with
  file paths.

## `hash-password.sh`

Prints a bcrypt hash for the `password_hash` field in `config/users.json`.

```bash
scripts/hash-password.sh                        # prompts twice, no echo
printf '%s' "$PW" | scripts/hash-password.sh    # reads stdin, so the password is not in argv or shell history
COST=13 scripts/hash-password.sh                # work factor 10 to 31, default 12
```

bcrypt uses only the first 72 bytes of a password. Use a random password of at most 72 bytes:
`openssl rand -base64 33`.

## TLS

`scripts/tls/` makes certificates for the server's TLS listener. The `distlock-health tls` command does the
same without `openssl` (`new-ca`, `use-ca`, `server-cert`, same defaults and options), so on a machine that has the
binary you do not need these scripts. Keep using the scripts for a key or CA key that has a passphrase, which the
binary does not read. Passphrase-free keys and certificates from either are interchangeable. Run any script with `--help` for its
options. Each one writes to `config/tls` by default, relative to the directory you run it from.
Pass `--dir` to choose another folder.

| Script | Purpose |
|---|---|
| `new-ca.sh` | Creates a private CA: `ca.crt` for clients, `ca.key` to keep secret. |
| `use-ca.sh --cert FILE [--key FILE]` | Uses a CA you already have. |
| `server-cert.sh --name HOST --ip ADDR` | Creates `server.crt` and `server.key` for the names and addresses clients use. |

Run them from the repository root to match the `config/tls` paths in `.env.example`.

## Troubleshooting

| Problem | Cause and fix |
|---|---|
| `cargo not found: Cargo.lock not refreshed` | Install Rust, or run `cargo update --workspace` yourself. |
| `check-versions.sh` shows a package in red after a bump | An edit was missed, or the package was meant to differ. Check the file named in the row. |
| `set-versions.sh` says nothing changed | The line did not match the expected format. Check the manifest and the table above. |
| `test.sh python` cannot copy the built library on an external drive | `maturin develop` can fail across volumes. Use a local checkout, or build into the project's own `target/`. See the `DESIGN.md` testing notes. |
