# distlock

[![CI](https://github.com/loggerheads-with-binary/distlock/actions/workflows/ci.yml/badge.svg)](https://github.com/loggerheads-with-binary/distlock/actions/workflows/ci.yml)
[![Release](https://github.com/loggerheads-with-binary/distlock/actions/workflows/release.yml/badge.svg)](https://github.com/loggerheads-with-binary/distlock/actions/workflows/release.yml)
[![codecov](https://codecov.io/gh/loggerheads-with-binary/distlock/graph/badge.svg)](https://codecov.io/gh/loggerheads-with-binary/distlock)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A small distributed lock service: a TCP server and clients for Rust and Python. It offers mutexes,
read/write locks, semaphores and broadcast events. Every lock is a lease that the client renews in the
background, so a crashed holder's lock frees itself.

## What is in this repository

| Package | What it is | Install | Coverage |
|---|---|---|---|
| [`distlock-server`](crates/distlock-server) | the lock server (binary and library) | `cargo install distlock-server` | [![codecov](https://codecov.io/gh/loggerheads-with-binary/distlock/graph/badge.svg?component=distlock-server)](https://codecov.io/gh/loggerheads-with-binary/distlock) |
| [`distlock-client`](crates/distlock-client) | async and blocking Rust client | `cargo add distlock-client`, health check: `cargo install distlock-client --bin distlock-health` | [![codecov](https://codecov.io/gh/loggerheads-with-binary/distlock/graph/badge.svg?component=distlock-client)](https://codecov.io/gh/loggerheads-with-binary/distlock) |
| [`distlock-core`](crates/distlock-core) | the pure lock-table state machine | `cargo add distlock-core` | [![codecov](https://codecov.io/gh/loggerheads-with-binary/distlock/graph/badge.svg?component=distlock-core)](https://codecov.io/gh/loggerheads-with-binary/distlock) |
| [`distlock-proto`](crates/distlock-proto) | protobuf wire types | `cargo add distlock-proto` | [![codecov](https://codecov.io/gh/loggerheads-with-binary/distlock/graph/badge.svg?component=distlock-proto)](https://codecov.io/gh/loggerheads-with-binary/distlock) |
| [`pydistlock`](python) | Python client (imports as `distlock`) | `pip install pydistlock` | [![codecov](https://codecov.io/gh/loggerheads-with-binary/distlock/graph/badge.svg?component=pydistlock)](https://codecov.io/gh/loggerheads-with-binary/distlock) |

## Install
Two programs: `distlock-server` and the health check `distlock-health` (in the `distlock-client`
package). Pick **one** of these ways. All of them put the binaries in `~/.cargo/bin` unless you say
otherwise, so make sure that folder is on your `PATH`.

**A. From crates.io**, with no checkout. `cargo install` takes the *package* name; no repository name
or URL is needed (`--bin` picks the program inside the `distlock-client` package). It works once a
release containing the program is published.
```bash
cargo install --locked distlock-server
cargo install --locked distlock-client --bin distlock-health
```
**OR B. From a checkout** of this repository (a local path; `git clone https://github.com/loggerheads-with-binary/distlock`
first). To skip the clone, `cargo install --git https://github.com/loggerheads-with-binary/distlock distlock-server`
does the same; that is the one form that needs the repository URL.
```bash
cargo install --locked --path crates/distlock-server
cargo install --locked --path crates/distlock-client --bin distlock-health
```
**OR C. Download a prebuilt binary** from the [Releases page](https://github.com/loggerheads-with-binary/distlock/releases).
Each version has `distlock-<version>-<target>.tar.gz` (`.zip` on Windows) for Linux (static, x86_64 and
aarch64), macOS (Intel and Apple silicon) and Windows (x64), plus a `SHA256SUMS` file. Every archive holds
`distlock-health`; the Linux and macOS ones also hold `distlock-server` (the Windows archive has the health
check only).
```bash
curl -LO https://github.com/loggerheads-with-binary/distlock/releases/download/v0.3.0/distlock-0.3.0-x86_64-unknown-linux-musl.tar.gz
tar -xzf distlock-0.3.0-x86_64-unknown-linux-musl.tar.gz && install -m 755 distlock-0.3.0-*/distlock-* /opt/distlock/bin/
```
**OR D. Build it yourself** and copy the binaries wherever you like.
```bash
cargo build --release -p distlock-server -p distlock-client --bin distlock-server --bin distlock-health
install -m 755 target/release/distlock-server target/release/distlock-health /opt/distlock/bin/
```
**Installing at a given location.** With A or B add `--root DIR`: the programs land in `DIR/bin`, for
example `cargo install --locked distlock-server --root /opt/distlock` gives
`/opt/distlock/bin/distlock-server`. (`CARGO_INSTALL_ROOT=/opt/distlock` does the same.) With C or D, copy the binaries (from the archive, or from
`target/release/`) to any folder. To remove an A/B install, `cargo uninstall distlock-server --root DIR`.

## Quick start

**1. Run the server** (installed as described above). It reads its settings from environment variables (see
[`.env.example`](.env.example)) and its users from a JSON file of bcrypt hashes
([`config/users.example.json`](config/users.example.json), hashes made with
[`scripts/hash-password.sh`](scripts/hash-password.sh)).

```bash
cp .env.example .env                  # defaults listen on 127.0.0.1:7878
cp config/users.example.json config/users.json   # then replace every hash
distlock-server
```

**2. Use it from Python.**

```python
from distlock import DistLockClient, ProcessingStatus

client = DistLockClient.from_url("distlock://billing:s3cret@127.0.0.1:7878/billing")

async with client.Mutex("nightly-report") as guard:
    if guard.status is ProcessingStatus.SUCCEEDED:
        return                                    # the previous holder already did the work
    ...                                           # the lease is renewed while this runs
    guard.set_status(ProcessingStatus.SUCCEEDED)  # recorded when the block ends
```

**3. Or from Rust.**

```rust
use distlock_client::{Acquire, DistLockClient, State};

let client = DistLockClient {
    client_id: "billing".into(),
    password: "s3cret".into(),
    ..Default::default()
}
.connect()
.await?;

let lock = client.acquire(Acquire::mutex("nightly-report")).await?;
// ... work; `lock.is_lost()` tells you if the lease was lost ...
lock.release_with(Some(State::Succeeded), None).await?;
```

## Health check and monitoring
`distlock-health` logs in and prints the server's memory, CPU, open files, load, connected clients, locks
held and startup-grace state. It can run on a timer and POST the result to a webhook.
```bash
# install it as described in Install above
export DISTLOCK_URL='distlock://billing:s3cret@127.0.0.1:7878'
distlock-health                                  # one check; exit 0 healthy, 1 failed, 3 in startup grace (--require-ready)
distlock-health --interval 30 \
  --ok-webhook https://hooks.example.com/up \
  --error-webhook https://hooks.example.com/down  # or $DISTLOCK_OK_WEBHOOK / $DISTLOCK_ERROR_WEBHOOK
```
Every check logs in afresh and POSTs a JSON report to the OK or the error webhook. Full options, the
JSON format and exit codes are in the [`distlock-client` README](crates/distlock-client/README.md#distlock-health).
From Python, `await client.stats()` (or `client.stats_sync()`) returns the same figures.

## TLS with your own certificate and CA
TLS is off until you set `DISTLOCK_TLS_ENABLED=true`. The server then needs a certificate and key and
refuses to start without a valid pair. The scripts in [`scripts/tls/`](scripts/tls) make them (they need only `openssl`):
```bash
scripts/tls/new-ca.sh                      # a private CA: config/tls/ca.crt (for clients) + ca.key (keep secret)
#   OR use a CA you have:  scripts/tls/use-ca.sh --cert /pki/ca.crt --key /pki/ca.key
scripts/tls/server-cert.sh --name locks.internal.example --ip 10.0.0.5     # server.crt + server.key
#   several: --name a.example,b.example --name '*.svc.example' --ip 10.0.0.5,10.0.0.6   (wildcards ok; no CIDR ranges)
#   already have a key?  add --key /old/server.key
DISTLOCK_TLS_ENABLED=true DISTLOCK_TLS_CERT_FILE=config/tls/server.crt DISTLOCK_TLS_KEY_FILE=config/tls/server.key distlock-server
```
Give `ca.crt` to the clients, as a file or as text (set one):
```python
DistLockClient.from_url("distlock://billing:s3cret@10.0.0.5:7878?tls=true&ca_file_path=/etc/distlock/ca.crt")
DistLockClient.from_url(url, tls=True, ca_file_content=ca_pem_text)      # arguments beat the URL
```
Clients check the certificate in two independent ways, both on by default: `tls_verify_ca` (chains to a
trusted CA) and `tls_verify_host` (names the host or IP in the URL). Turning off one gives "CA only" or
"name only"; both off encrypts without authenticating the server. With a public CA's certificate clients need
no CA file. The private key is always required, but never a passphrase: the server cannot ask for one.
Renewal, an external CA, and a table of error messages are in the [server README](crates/distlock-server/README.md#tls);
the client side is in the [Rust](crates/distlock-client/README.md#tls) and [Python](python/README.md) READMEs.

## Things to know before you deploy

* **TLS is optional and off by default.** Without `DISTLOCK_TLS_ENABLED=true` the
  server speaks plaintext: bind it to a private interface and never expose it on an untrusted network.
  Clients choose how strictly to verify the certificate with two flags (`tls_verify_ca`, `tls_verify_host`);
  see the [server](crates/distlock-server/README.md#tls) and [Python](python/README.md) READMEs.
* **State is in memory.** After a restart the server refuses lock requests for `DISTLOCK_STARTUP_GRACE_MS`
  so that leases from the old process run out. Keep that value at or above `DISTLOCK_MAX_TTL_MS`.
* **Use fencing tokens.** Every grant carries a strictly increasing token. Pass it to the resource you
  protect and reject older ones. A lease alone cannot stop a paused process from acting late.

## Building and releasing

See [`BUILD.md`](BUILD.md). Releases publish the four crates to crates.io, `pydistlock` to PyPI, and the binaries and wheels to GitHub Releases
when a `vX.Y.Z` tag is pushed. Pre-commit hooks are in `.pre-commit-config.yaml`.

## License

[MIT](LICENSE)
