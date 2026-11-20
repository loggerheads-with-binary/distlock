# distlock-core

[![CI](https://github.com/loggerheads-with-binary/distlock/actions/workflows/ci.yml/badge.svg)](https://github.com/loggerheads-with-binary/distlock/actions/workflows/ci.yml)
[![Release](https://github.com/loggerheads-with-binary/distlock/actions/workflows/release.yml/badge.svg)](https://github.com/loggerheads-with-binary/distlock/actions/workflows/release.yml)
[![codecov](https://codecov.io/gh/loggerheads-with-binary/distlock/graph/badge.svg?component=distlock-core)](https://codecov.io/gh/loggerheads-with-binary/distlock)
[![crates.io](https://img.shields.io/crates/v/distlock-core.svg)](https://crates.io/crates/distlock-core)
[![docs.rs](https://docs.rs/distlock-core/badge.svg)](https://docs.rs/distlock-core)

The lock-table state machine behind [distlock](https://github.com/loggerheads-with-binary/distlock).
It is pure and synchronous: no IO, no clock, no async runtime. You pass the current time into every
call, which makes the lock semantics easy to test, including with property tests.

It supports mutexes, read/write locks (fair, FIFO), semaphores, leases with extension, fencing tokens
and a status left by one holder for the next.

```toml
[dependencies]
distlock-core = "0.2"
```

```rust
use std::time::Duration;
use distlock_core::*;

let mut table = LockTable::new(Config::default());
let now = Duration::ZERO;
let status = table.acquire(now, AcquireRequest {
    session: SessionId(1),
    request: RequestId(1),
    name: "jobs/nightly".into(),
    want: Want::Mutex,
    ttl: Duration::from_secs(30),
    wait: Duration::from_secs(5),
}).unwrap();
let AcquireStatus::Granted(grant) = status else { unreachable!() };
table.release(now, SessionId(1), "jobs/nightly", grant.token, Some(ReleaseState::Succeeded), None).unwrap();
```

Most users want the server ([`distlock-server`](https://crates.io/crates/distlock-server)) and a client
([`distlock-client`](https://crates.io/crates/distlock-client)) instead of this crate.

Licensed under the [MIT license](https://github.com/loggerheads-with-binary/distlock/blob/main/LICENSE).
