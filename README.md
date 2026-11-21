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

## Quick start

**1. Run the server.** It reads its settings from environment variables (see
[`.env.example`](.env.example)) and its users from a JSON file of bcrypt hashes
([`config/users.example.json`](config/users.example.json), hashes made with
[`scripts/hash-password.sh`](scripts/hash-password.sh)).

```bash
cargo install distlock-server
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
cargo install --path crates/distlock-client --bin distlock-health   # from a checkout; or cargo build --release -p distlock-client --bin distlock-health
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
Pass the server a certificate and key, and tell clients which CA to trust. For an internal service the
simplest setup is your own CA:
```bash
KEY="-newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -pkeyopt ec_param_enc:named_curve -nodes"
openssl req -x509 $KEY -days 3650 -keyout ca.key -out ca.crt -subj "/CN=distlock CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
openssl req -new $KEY -keyout server.key -out server.csr -subj "/CN=distlock-server"
printf 'subjectAltName=DNS:locks.internal.example,IP:10.0.0.5\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -sha256 -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 365 -extfile server.ext -out server.crt
DISTLOCK_TLS_CERT_FILE=server.crt DISTLOCK_TLS_KEY_FILE=server.key distlock-server   # keep ca.key offline
```
Give `ca.crt` to the clients, as a file or as text. Either field works; set one:
```python
DistLockClient.from_url("distlock://billing:s3cret@10.0.0.5:7878?tls=true&ca_file_path=/etc/distlock/ca.crt")
DistLockClient(client_id="billing", password="s3cret", host="10.0.0.5", tls=True, ca_file_content=ca_pem_text)
```
Clients check the certificate in two independent ways, both on by default: `tls_verify_ca` (chains to a
trusted CA) and `tls_verify_host` (names the host or IP in the URL, via `subjectAltName`). Turning off
one gives "CA only" or "name only"; both off encrypts without authenticating the server. With a public
CA's certificate clients need no CA file. The full guide, with renewal, intermediate CAs and a table of
error messages, is in the [server README](crates/distlock-server/README.md#tls); the client side is in the
[Rust](crates/distlock-client/README.md#tls) and [Python](python/README.md) READMEs.

## Things to know before you deploy

* **TLS is optional and off by default.** Without `DISTLOCK_TLS_CERT_FILE` and `DISTLOCK_TLS_KEY_FILE` the
  server speaks plaintext: bind it to a private interface and never expose it on an untrusted network.
  Clients choose how strictly to verify the certificate with two flags (`tls_verify_ca`, `tls_verify_host`);
  see the [server](crates/distlock-server/README.md#tls) and [Python](python/README.md) READMEs.
* **State is in memory.** After a restart the server refuses lock requests for `DISTLOCK_STARTUP_GRACE_MS`
  so that leases from the old process run out. Keep that value at or above `DISTLOCK_MAX_TTL_MS`.
* **Use fencing tokens.** Every grant carries a strictly increasing token. Pass it to the resource you
  protect and reject older ones. A lease alone cannot stop a paused process from acting late.

## Building and releasing

See [`BUILD.md`](BUILD.md). Releases publish the four crates to crates.io and `pydistlock` to PyPI
when a `vX.Y.Z` tag is pushed. Pre-commit hooks are in `.pre-commit-config.yaml`.

## License

[MIT](LICENSE)
