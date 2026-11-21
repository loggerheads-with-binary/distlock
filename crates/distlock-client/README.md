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

## `distlock-health`

A command-line health check. It logs in to a server and prints memory, CPU (whole machine and the
server process, each 0–100 averaged over all cores), open files, load, connected clients, locks held,
queued requests and whether the server is still in its startup grace period. It can also run forever
and report to webhooks.

### Install or build

```sh
# from crates.io (needs a release of distlock-client that includes the tool)
cargo install distlock-client --bin distlock-health

# from a checkout of this repository
cargo install --path crates/distlock-client --bin distlock-health   # installs to ~/.cargo/bin

# or just build it, without installing
cargo build --release -p distlock-client --bin distlock-health      # target/release/distlock-health
```

The tool is behind the `cli` feature, which is on by default. A library user who does not want its
dependencies (`ureq` for webhooks, `serde_json`) writes
`distlock-client = { version = "0.2", default-features = false }`.

### Run it

The server is a URL, `distlock://CLIENT_ID:PASSWORD@HOST[:PORT]`. User and password may be
percent-encoded. It is read from `--url URL`, `--url -` (one line on stdin), `$DISTLOCK_URL`, or
stdin when piped. Prefer the last three: a password in argv shows up in `ps`.

```sh
export DISTLOCK_URL='distlock://billing:s3cret@127.0.0.1:7878'
distlock-health                       # one check, prints key: value lines
distlock-health --require-ready       # exit 3 while the server is in its startup grace period
echo "$URL" | distlock-health         # URL from stdin
```

| Flag | Meaning |
|---|---|
| `--url URL` or `--url -` | the server (see above) |
| `--require-ready` | treat the startup grace period as unhealthy (exit 3, error webhook) |
| `--timeout SECS` | give up on one check after this long (default 15) |
| `--interval SECS` | keep checking, waiting this long after each check finishes |
| `--ok-webhook URL` | POST here when a check is healthy (also `$DISTLOCK_OK_WEBHOOK`) |
| `--error-webhook URL` | POST here when a check fails (also `$DISTLOCK_ERROR_WEBHOOK`) |

One-shot exit codes: `0` healthy, `1` could not connect, log in or get stats, `2` bad usage, `3` in
startup grace with `--require-ready`.

### Continuous monitoring and webhooks

With `--interval`, the tool checks forever. Every check logs in afresh, so a check also proves that
logging in still works. A failed check never stops the loop. Webhooks also work without `--interval`,
for a cron job.

```sh
distlock-health --interval 30 --require-ready \
  --ok-webhook    https://hooks.example.com/distlock/up \
  --error-webhook https://hooks.example.com/distlock/down
```

Each check POSTs one JSON document (`Content-Type: application/json`) to the matching webhook, if one
is set. Both are optional, and every check reports, so a healthy server posts to the OK webhook every
interval. A check is an error if it cannot connect or log in, times out, or finds the server in its
grace period with `--require-ready`.

```json
{
  "status": "ok",
  "text": "distlock 10.0.0.5:7878: healthy",
  "server": "10.0.0.5:7878",
  "checked_at_unix": 1795287325,
  "error": null,
  "stats": { "ready": true, "connected_clients": 3, "locks_held": 2, "process_cpu_percent": 0.4, "...": "..." }
}
```

On `"status": "error"`, `error` says why, and `stats` is `null` unless the server answered. `text`
is a single line for chat tools that show one string. Webhook URLs are `http://` or `https://`. They
often hold a secret, so the tool never prints them. A webhook that is down or answers an error code
is reported on stderr and does not change the exit code or stop the loop. The login password is never
sent.
