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

## Primitives and defaults

A *primitive* (`MutexPrimitive`, `ReadLockPrimitive`, `WriteLockPrimitive`, `SemaphorePrimitive`; also
`Acquire`, `EventWait` and `EventSet`) describes what you want without a client, so one definition can be
bound again and again, to one client or to several. Bind it with `Primitive::bind`, or let the client
attach it with `Attach::attach` (blanket-implemented for everything that is `AsRef<Client>`; the blocking
client uses `AttachBlocking`):

```rust
use distlock_client::{Attach, Primitive, ReadLockPrimitive};

let readers = ReadLockPrimitive::new("catalog").ttl(Duration::from_secs(20));
let lock = readers.bind(&client).await?;
let lock = other_client.attach(&readers).await?;
```

What a request leaves out follows the process-wide defaults in `distlock_client::defaults`, read when the
request is made. They are lock-free atomics:

```rust
use distlock_client::defaults::{set_default_semaphore_wait, set_default_wait};

set_default_wait(Some(Duration::from_secs(10)));            // locks and events
set_default_semaphore_wait(Some(Duration::from_secs(60)));  // semaphores wait longer
```

A kind's default beats the general one, and the request's own value beats both. `Wait::Forever`
(`.wait_forever()`) waits as long as the server allows even when a default is set; `reset_default_*()` and
`reset_all()` forget defaults. Unset, nothing changes. `Acquire::wait` and `EventWait::wait` are now of
type `Wait` (`Default`, `Forever`, `For(Duration)`); the `.wait(Duration)` builders are unchanged.

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
including renewal, an external CA and the table of error messages.

URL form: `distlock://user:pass@host:7878?tls=true&tls_verify_ca=true&tls_verify_host=false&ca_file_path=%2Fetc%2Fca.crt`.
`distlock-health check` and `distlock-health reset-users` take `--tls`, `--ca-file PEM`, `--no-verify-ca` and
`--no-verify-host`. The same tool makes the certificates: `distlock-health tls`.

## Logging in with a key

`auth_mode: 1` (the default) sends `password`. With TLS off that crosses the network in the clear.
`auth_mode: 2` logs in with an Ed25519 key and sends no password: `password` then holds the 32-byte
**private key** in base64 (43 characters URL-safe; the standard alphabet works too), and the client signs a
nonce the server sends. The server must list the matching public key under the user's `public_keys`. Make a
pair with `python -m distlock.keys` (`pip install "pydistlock[keys]"`), see the
[server README](https://github.com/loggerheads-with-binary/distlock/blob/main/crates/distlock-server/README.md#logins-passwords-and-keys).

```rust
let client = DistLockClient {
    auth_mode: 2,
    password: std::env::var("DISTLOCK_PRIVATE_KEY")?, // a secret: not in source control, not in a command line
    min_version: Some("0.3.0".into()),
    ..DistLockClient::from_url("distlock://billing:x@10.0.0.5:7878")?
}
.connect()
.await?;
// or in a URL: distlock://billing:<private key>@10.0.0.5:7878?auth_mode=v2&min_version=0.3.0
```

`min_version` is the oldest server release you accept (default `0.3.0`, the release this client was built
for). It is checked right after logging in; an older server fails `connect` with `Error::ServerTooOld`. A server
that does not report a version (0.3.0 and earlier) is taken to be 0.3.0. A wrong key, an unknown user and an
expired key all fail with the same `Error::Auth`. A server that requires keys (`DISTLOCK_MIN_AUTH_MODE=2`)
refuses `auth_mode: 1` with a message saying so. A signature does not protect the session that follows:
use TLS as well.

## `distlock-health`

The operator's tool for a distlock deployment, one binary with explicit subcommands:

| Command | What it does |
|---|---|
| [`check`](#check) | Logs in to a server and prints its health figures; can run forever and report to webhooks. |
| [`reset-users`](#reset-users) | Makes the server re-read its users file; prints what changed and the file's MD5. |
| [`tls new-ca`, `tls use-ca`, `tls server-cert`](#tls-certificates) | Makes the certificates for the server's TLS listener, without `openssl`. |
| [`keys generate`](#keys-generate) | Makes an Ed25519 key pair for `auth_mode=2` logins, without Python. |

Nothing happens without a subcommand: `distlock-health` on its own prints this list and exits with
status 2, and so do old-style flags without one. The URL is no longer read from a pipe unless you say
`--url -`.

Both `check` and `reset-users` log in as an ordinary client, so they need a user in `users.json`.

### Install or build

Pick **one**. `cargo install` puts the program in `~/.cargo/bin` (keep it on your `PATH`) unless you
add `--root DIR`, which puts it in `DIR/bin`.

**A. From crates.io.** `cargo install` takes the *package* name, `distlock-client`; no repository is
needed. `--bin` selects the program inside it. Works once a release containing the tool is published.
```sh
cargo install --locked distlock-client --bin distlock-health
cargo install --locked distlock-client --bin distlock-health --root /opt/distlock   # /opt/distlock/bin/
```
**OR B. From a checkout** (`git clone https://github.com/loggerheads-with-binary/distlock`, then from its
root). `cargo install --git https://github.com/loggerheads-with-binary/distlock distlock-client --bin distlock-health`
skips the clone.
```sh
cargo install --locked --path crates/distlock-client --bin distlock-health [--root /opt/distlock]
```
**OR C. Build it** and copy the binary anywhere.
```sh
cargo build --release -p distlock-client --bin distlock-health   # target/release/distlock-health
install -m 755 target/release/distlock-health /opt/distlock/bin/
```

The tool is behind the `cli` feature, which is on by default. A library user who does not want its
dependencies (`clap`, `rcgen`, `x509-parser`, `ureq` for webhooks, `serde_json`, `time`) writes
`distlock-client = { version = "0.4", default-features = false }`. The tool adds about 1 MB to the
release binary compared with the old health check alone (4.4 MB stripped, on macOS arm64).

### The server: `--url`

`check` and `reset-users` name the server with a URL, `distlock://CLIENT_ID:PASSWORD@HOST[:PORT]`. User
and password may be percent-encoded. It is **required**, from `--url URL`, from `--url -` (one line on
stdin), or from `$DISTLOCK_URL`. Prefer the last two: a password in argv shows up in `ps`. The same
connection flags apply to both commands:

| Flag | Meaning |
|---|---|
| `--url URL` or `--url -` | the server (see above); else `$DISTLOCK_URL` |
| `--tls`, `--ca-file PEM`, `--no-verify-ca`, `--no-verify-host` | TLS and what to check (see [TLS](#tls)); the flags only add TLS or relax a check |
| `--timeout SECS` | give up after this long (default 15) |

Exit codes: `0` success, `1` the action failed (could not connect, log in, or get an answer), `2` bad
usage, `3` (`check --require-ready` only) the server is in its startup grace period.

### `check`

Logs in, asks for the server's figures and prints them as `key: value` lines. One-shot by default.

```sh
export DISTLOCK_URL='distlock://billing:s3cret@127.0.0.1:7878'
distlock-health check                       # one check
distlock-health check --require-ready       # exit 3 while the server is in its startup grace period
distlock-health check --human-readable      # 16.4 GiB, 24.3%, 92_160
echo "$URL" | distlock-health check --url -   # URL from stdin
```

| Flag | Meaning |
|---|---|
| `--require-ready` | treat the startup grace period as unhealthy (exit 3, error webhook) |
| `--human-readable` | sizes as `16.4 GiB`, percentages as `24.3%`, integers as `92_160` (process ids are left alone). Only the text output changes: webhook JSON keeps plain numbers |
| `--interval SECS` | keep checking, waiting this long after each check finishes |
| `--ok-webhook URL` | POST here when a check is healthy (also `$DISTLOCK_OK_WEBHOOK`; empty means none) |
| `--error-webhook URL` | POST here when a check fails (also `$DISTLOCK_ERROR_WEBHOOK`; empty means none) |

`users_md5` is the MD5 of the users file the server has in force (32 hex characters, `n/a` from a server
that predates it). It changes only when the server loads or reloads the file, so a rolling change of
users is done once every server reports the hash of the file you rolled out. MD5 is a change detector
here, not a security measure.

### Continuous monitoring and webhooks

With `--interval`, `check` runs forever. Every check logs in afresh, so a check also proves that
logging in still works. A failed check never stops the loop. Webhooks also work without `--interval`,
for a cron job.

```sh
distlock-health check --interval 30 --require-ready \
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
  "stats": { "ready": true, "users_md5": "9d0e...", "connected_clients": 3, "locks_held": 2, "process_cpu_percent": 0.4, "...": "..." }
}
```

On `"status": "error"`, `error` says why, and `stats` is `null` unless the server answered. `text`
is a single line for chat tools that show one string. Webhook URLs are `http://` or `https://`. They
often hold a secret, so the tool never prints them. A webhook that is down or answers an error code
is reported on stderr and does not change the exit code or stop the loop. The login password is never
sent.

### `reset-users`

Makes the server re-read its users file and swap it in, then prints what changed and the MD5 of the
file now in force. Logins are never refused meanwhile.

```sh
distlock-health reset-users
# users_md5: 9d0e5b58f6f0e2d6a3c1b6a4f1d7c2aa
# added: 1
# removed: 0
# disabled: 0
# enabled: 0
# password_changed: 0
# sessions_closed: 0
# connections_closed: 0
# invoker_dropped: false
```

* The server also writes the hash to its log (`users_md5=...`), at start-up and on every reset, including
  a reset that found the file unchanged.
* A file that cannot be loaded changes nothing: the old users stay in force, the command exits 1, and
  the details are in the server's log.
* Removing or disabling a user ends that user's sessions and releases its locks after the lock-delay.
  If the user in the URL is one of them, the report still arrives and `invoker_dropped` is `true`.
* There is no per-user permission yet: any logged-in client may run it. Keep the port private.
* `password_changed` counts every user whose hash text differs, and bcrypt salts every hash, so
  re-hashing an unchanged password counts. Do not use it as the change signal: use `users_md5`.

A rolling change of users, with the server already running:

```sh
scripts/hash-password.sh                       # or: your own way to make the bcrypt hash
$EDITOR config/users.json
md5sum config/users.json                        # macOS: md5 -q config/users.json
distlock-health reset-users | grep users_md5   # must show the same hash
distlock-health check | grep users_md5         # the same, from the stats
```

The server has to be able to read the file at its `DISTLOCK_USERS_FILE` path, which is a path on the
server's machine: in a cluster, update the file on each server before resetting it, then compare
each server's `users_md5` with the hash of the file you rolled out.

### TLS certificates

`distlock-health tls` makes what the server's TLS listener needs, without `openssl`. It does the same as
the scripts in `scripts/tls/`. Files go to `--dir` (default `config/tls`, relative to where you run it),
created if missing. Nothing existing is overwritten unless you pass `--force`, and private keys are
written readable by their owner only.

```sh
distlock-health tls new-ca --dir config/tls                  # ca.key (secret) and ca.crt (for clients)
distlock-health tls server-cert --dir config/tls \
    --name locks.internal.example --ip 10.0.0.5              # server.crt and server.key
#   several: --name a.example,b.example --name '*.svc.example' --ip 10.0.0.5,10.0.0.6
distlock-health tls use-ca --cert /pki/ca.crt --key /pki/ca.key    # use a CA you already have instead
```

| Command | Options |
|---|---|
| `tls new-ca` | `--dir`, `--cn NAME` (default "distlock CA"), `--days N` (default 3650), `--force` |
| `tls use-ca` | `--cert FILE` (required), `--key FILE`, `--dir`, `--force`. It checks that the file is a CA that has not expired and that the key belongs to it |
| `tls server-cert` | `--name HOST` and `--ip ADDRESS` (at least one; comma-separated, repeatable), `--dir`, `--days N` (default 365), `--key FILE` (certify an existing key), `--force` |

* List every name and address that clients put in their URL: they are checked against the certificate's
  `subjectAltName`. A wildcard is `*.example.com` and covers one label (not `example.com`, not
  `a.b.example.com`); `*.com` and IP ranges are refused.
* With a CA certificate but no CA key (`use-ca --cert` alone), `server-cert` writes `server.csr` for your
  CA to sign instead; save what it returns as `server.crt`.
* If the CA is an intermediate, its certificate is appended to `server.crt`, and clients trust the root.
* Keys are ECDSA P-256 in PKCS#8 PEM. A key you give (`--key`) must be PKCS#8 PEM and **must not have a
  passphrase**: the server cannot ask for one, and this tool cannot either. Remove it with
  `openssl pkey -in old.key -out plain.key`. The scripts in `scripts/tls/` do accept passphrase-protected
  keys and `new-ca.sh --encrypt-key`; this tool does not.
* Certificates are valid from five minutes ago, so a client whose clock is slightly behind accepts them.

Then start the server with `DISTLOCK_TLS_ENABLED=true DISTLOCK_TLS_CERT_FILE=... DISTLOCK_TLS_KEY_FILE=...`
and give clients `ca.crt`. See [TLS in the server README](https://github.com/loggerheads-with-binary/distlock/blob/main/crates/distlock-server/README.md#tls).

### `keys generate`

Makes an Ed25519 key pair for `auth_mode=2` logins, the same as `python -m distlock.keys`.

```sh
distlock-health keys generate --client-id billing --identifier laptop --expiry 2026-12-31T00:00:00Z
```

It prints the private key (the client's password with `auth_mode=v2`; a secret, so only to stdout), the
public key, the entry to add under the user's `public_keys` in `users.json`, and the client URL.
`--identifier` is a name for the key in the server log, `--issuer` is metadata, and `--expiry` is an
RFC 3339 time with an offset (checked here, since the server refuses a file with a bad one). Both halves
are 43 characters of URL-safe base64. Keys are random, so run it again for another pair.
