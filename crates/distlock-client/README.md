# distlock-client

[![CI](https://github.com/loggerheads-with-binary/distlock/actions/workflows/ci.yml/badge.svg)](https://github.com/loggerheads-with-binary/distlock/actions/workflows/ci.yml)
[![Release](https://github.com/loggerheads-with-binary/distlock/actions/workflows/release.yml/badge.svg)](https://github.com/loggerheads-with-binary/distlock/actions/workflows/release.yml)
[![codecov](https://codecov.io/gh/loggerheads-with-binary/distlock/graph/badge.svg?component=distlock-client)](https://codecov.io/gh/loggerheads-with-binary/distlock)
[![crates.io](https://img.shields.io/crates/v/distlock-client.svg)](https://crates.io/crates/distlock-client)
[![docs.rs](https://docs.rs/distlock-client/badge.svg)](https://docs.rs/distlock-client)

Async and blocking Rust client for the [distlock](https://github.com/loggerheads-with-binary/distlock)
lock server. It supports mutexes, read/write locks, semaphores and events.

```toml
[dependencies]
distlock-client = "0.2"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust
use distlock_client::{Acquire, DistLockClient, State};

#[tokio::main]
async fn main() -> distlock_client::Result<()> {
    let client = DistLockClient {
        client_id: "billing-service".into(),
        password: "secret".into(),
        ..Default::default()
    }
    .connect()
    .await?;

    let lock = client.acquire(Acquire::mutex("nightly-report")).await?;
    if lock.previous().map(|p| p.state) == Some(State::Succeeded) {
        // somebody already did the work
    }
    // ... work; the lease is renewed for you, and `lock.is_lost()` tells you if it was lost ...
    lock.release_with(Some(State::Succeeded), None).await?;
    Ok(())
}
```

* **Async and blocking.** `DistLockClient::connect` gives the async `Client`. `connect_blocking` gives
  `blocking::Client`, with the same operations.
* **One session, a pool of connections.** Dropped connections are re-established and the session is
  re-joined. The locks you hold are re-verified against the server.
* **Leases renew themselves** while you hold the lock.
* **Lock names** are sent as `namespace:family:name` (family `mutex`, `rw` or `sem`), so a mutex and a
  read/write lock with the same name never conflict.

The server is [`distlock-server`](https://crates.io/crates/distlock-server). Python users want
[`pydistlock`](https://pypi.org/project/pydistlock/), which wraps this crate.

Licensed under the [MIT license](https://github.com/loggerheads-with-binary/distlock/blob/main/LICENSE).
