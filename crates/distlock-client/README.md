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

## TLS

Set `tls: true` (or `?tls=true` in a URL) to connect over TLS to a server that has a certificate.
Two independent flags choose what is checked on the server's certificate:

| `tls_verify_ca` | `tls_verify_host` | Mode | What it protects against |
|---|---|---|---|
| `true` (default) | `true` (default) | full verification | eavesdropping and impersonation |
| `true` | `false` | CA only | the certificate must chain to a trusted CA; its name is ignored |
| `false` | `true` | name only | the certificate must name `host` (DNS name or IP); any issuer, so self-signed passes. An attacker with their own certificate for that name is not stopped |
| `false` | `false` | none | eavesdropping only: the server is not authenticated |

The handshake signature is verified in every mode. A rejected certificate fails `connect` with
`Error::Tls`, whose message says why (`UnknownIssuer`, `NotValidForName`, `Expired`, ...).

### Trusting your own CA

By default the client trusts the public CAs built into it (Mozilla's list). A server certificate from
a private CA, or a self-signed one, is rejected until you hand the client the CA certificate, as a file
or as PEM text. Set one of the two, not both (`Error::Config` otherwise). It replaces the public roots
and may contain several certificates.

| Field | URL parameter | |
|---|---|---|
| `ca_file_path: Option<PathBuf>` | `ca_file_path=/etc/distlock/ca.crt` | PEM file of CA certificates |
| `ca_file_content: Option<String>` | `ca_file_content=-----BEGIN...` (percent-encoded) | the same, as text: for a CA kept in a secret or an environment variable |

```rust
let client = DistLockClient {
    tls: true,
    ca_file_path: Some("/etc/distlock/ca.crt".into()),
    ..DistLockClient::from_url("distlock://billing:s3cret@10.0.0.5:7878")?
}
.connect()
.await?;

// CA from an environment variable instead of a file:
let ca = std::env::var("DISTLOCK_CA_PEM")?;
let client = DistLockClient { tls: true, ca_file_content: Some(ca), ..base };
```

The host in the URL must be a name or IP address in the server certificate's `subjectAltName`.
`tls_verify_host: false` skips that check, `tls_verify_ca: false` skips the chain check (and with it
the need for a CA file). To create the CA and the server certificate, see
[TLS in the server README](https://github.com/loggerheads-with-binary/distlock/blob/main/crates/distlock-server/README.md#tls),
including the table of error messages.

URL form: `distlock://user:pass@host:7878?tls=true&tls_verify_ca=true&tls_verify_host=false&ca_file_path=%2Fetc%2Fca.crt`.
`distlock-health` takes `--tls`, `--ca-file PEM`, `--no-verify-ca` and `--no-verify-host`.

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
| `--tls`, `--ca-file PEM`, `--no-verify-ca`, `--no-verify-host` | TLS and what to check (see [TLS](#tls)); the flags only add TLS or relax a check |
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
