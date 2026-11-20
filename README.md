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
| [`distlock-client`](crates/distlock-client) | async and blocking Rust client | `cargo add distlock-client` | [![codecov](https://codecov.io/gh/loggerheads-with-binary/distlock/graph/badge.svg?component=distlock-client)](https://codecov.io/gh/loggerheads-with-binary/distlock) |
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

async with client.mutex("nightly-report") as guard:
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

## Things to know before you deploy

* **TLS is optional.** Set `DISTLOCK_TLS_CERT_FILE` and `DISTLOCK_TLS_KEY_FILE` on the server and `tls=true` on
  clients. Clients can skip certificate checks (`tls_verify=false`) while there is no trusted certificate; that
  encrypts but does not authenticate the server. Without TLS, bind to a private interface only.
* **State is in memory.** After a restart the server refuses lock requests for `DISTLOCK_STARTUP_GRACE_MS`
  so that leases from the old process run out. Keep that value at or above `DISTLOCK_MAX_TTL_MS`.
* **Use fencing tokens.** Every grant carries a strictly increasing token. Pass it to the resource you
  protect and reject older ones. A lease alone cannot stop a paused process from acting late.

## Building and releasing

See [`BUILD.md`](BUILD.md). Releases publish the four crates to crates.io and `pydistlock` to PyPI
when a `vX.Y.Z` tag is pushed. Pre-commit hooks are in `.pre-commit-config.yaml`.

## License

[MIT](LICENSE)
