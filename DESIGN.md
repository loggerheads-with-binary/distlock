# distlock — design (v1: single node, in-memory)

A distributed lock service written in Rust. Mutexes, RwLocks and semaphores, leased with TTLs,
spoken over TCP with a protobuf wire protocol.

## Scope of v1

| In | Out (deliberately deferred) |
|---|---|
| Single node, all state in memory | Multi-node / sharding (but the design keeps the door open) |
| bcrypt username/password auth, and Ed25519 key auth (auth mode 2) | Client certificates (mTLS) |
| Optional TLS (rustls, off by default) | Persistence (WAL / snapshots) |
| Authentication only (no per-user permissions) | RBAC / per-lock authorization |
| Protobuf over length-prefixed TCP | Re-entrancy (we do *reclaim*, see below), RwLock upgrade/downgrade |
| Mutex, RwLock, semaphore, and named broadcast events | |
| Stats and health reports (`Stats`, `distlock-health`) | |

## Identity model

```
client_id   = application name; the authenticated principal. Equals the username in users.json.
              Many processes (e.g. 10 pods of "billing") share one client_id.
session_id  = one per client *process* (its connection pool). THE OWNER of locks.
              Server-generated, never reused. Protected by an opaque session token.
connection  = a TCP connection attached to a session. Any connection of a session may carry
              any request for that session's locks (req #9).
fencing tok = u64 issued at every grant. Also the handle used to release / extend.
```

> **Interpretation to confirm:** a session spans *all* TCP connections of one client process. If
> `session_id` were per TCP connection, requirement #9 (any lock over any connection) could not hold,
> because a lock would be tied to the single connection that acquired it. Locks are never owned by
> `client_id`, otherwise two pods sharing an application name would not exclude each other.

There is no owner/task id. Consequence: **no re-entrancy.** If two tasks of the same process request
the same mutex, the second one queues behind the first like any other client. The client library
should guard against self-deadlock; the server enforces mutual exclusion between *grants*, not
between sessions.

## Connection and session lifecycle

1. **First connection:** `Hello{client_id, password}` → bcrypt verify → `HelloOk{session_id, session_token}`.
   Unknown users are verified against a dummy hash so timing does not reveal valid usernames.
   bcrypt runs on `spawn_blocking`.
   * **Auth mode 2 (keys).** The client sends `AuthStart{protocol_version, auth_mode = 2, client_id}` instead of
     `Hello`'s password. The server replies with `AuthChallenge{nonce}` (32 fresh random bytes per connection). The
     client answers with `AuthProof{signature}`, an Ed25519 signature over `"distlock-auth-v2\0" || nonce || client_id`.
     The server checks it against the user's `public_keys` (see *Users file rules*). A replayed signature fails
     because the nonce is new each time. The private key never reaches the server. The session token that follows is
     not protected by the signature, so keys without TLS still expose lock traffic.
   * `HelloOk` reports the server version and the accepted auth-mode range (`min_auth_mode..=max_auth_mode`). A client that
     asks for a mode outside that range gets `UnsupportedVersion`. `DISTLOCK_MIN_AUTH_MODE=2` refuses password logins.
   * The server version in `HelloOk` is what the client's `min_version` check reads.
2. **Extra pool connections and reconnects:** `Attach{session_id, session_token}`. No bcrypt.
   `AttachOk` returns the **session view** (held locks + pending acquires). That is *reclaim*:
   the client reconciles its in-flight futures against the view (held → resolve, pending → keep
   waiting, absent → it timed out/expired).
3. **TCP drop ≠ lock loss.** Locks live until their TTL. The session survives disconnects.
4. **`CloseSession`** is the graceful exit: releases every grant and cancels every pending acquire
   at once (state tag `ABANDONED`, no lock-delay).
5. **Session reaping:** a session with no connections, no grants and no pending requests is deleted
   after an idle timeout. Sessions with live grants live until the grants expire.
6. **Session token** is an opaque 256-bit random value checked against the server's session table
   (not a JWT). We are single-node and already hold the session state, so statelessness buys nothing,
   while a table lookup gives instant revocation. If multi-node arrives, switch to an HMAC-signed token.

Retries are idempotent: `Acquire.request_id` is client-chosen and unique per session. Re-sending a
request that is pending or granted returns its current state instead of queueing twice.
A dropped acquire future should send `CancelAcquire`: it dequeues a waiting request, or *releases*
the grant if it raced in. This closes the "granted to nobody" leak.

## Lock semantics

* **Namespace** is dynamic (Redis-style). A name is created on first acquire and dropped when it is
  idle: no holders, no waiters, lock-delay over, and no unexpired state tag.
* **A name has one kind** while it exists: `Mutex`, `RwLock`, or `Semaphore(capacity)`.
  Mixing kinds → `KIND_MISMATCH`. A different capacity → `CAPACITY_MISMATCH` (reports the existing one).
* **Fairness:** strict FIFO per name.
  * RwLock: readers are batched when consecutive at the head of the queue. A new reader queues
    behind a waiting writer, so writers cannot starve. If a waiting writer times out, readers
    behind it are admitted immediately.
  * Semaphore: head-of-line blocking is intentional (an N-permit request is not starved by 1-permit
    requests that arrive later).
* **Semaphores** are defined by the first acquirer (capacity travels with every acquire; later
  acquirers must match). When the last holder and waiter are gone the semaphore disappears, so a
  redeployed app can come back with a new capacity.
* **Acquire timeout** is mandatory in spirit: `wait_ms = 0` is try-lock; the server caps `wait_ms`.
  Timing out emits `WaitTimedOut`.
* **TTL:** every grant has a lease. `Extend` only ever *increases* the expiry, capped by
  `max_ttl` per call and `max_hold` in total (a stuck-but-alive client cannot renew forever).
  Extending an expired or re-granted lock fails with `NOT_HELD`; a lease is never resurrected.
  When a lease expires the server pushes `LeaseExpired` to the holder's connections.
* **Lock-delay:** after an *abnormal* loss (TTL expiry) of a Mutex/Write grant, the name is not
  granted to anyone for `lock_delay`, giving stale work time to drain. Graceful release and
  `CloseSession` do not trigger it.
* **Fencing tokens** come from one server-wide counter, not a per-lock counter. A per-lock counter
  would restart at 1 when an idle name is garbage-collected, silently breaking monotonicity.
  The counter is seeded from the wall clock at startup (`token_floor`) so tokens also survive a
  memory-only restart. Downstream resources must reject writes whose token is lower than one they
  have already seen; otherwise the lock is an efficiency lock, not a correctness lock.

### Holder state tag (req #12)

The holder may release with a tag: `SUCCEEDED | FAILED | STORED`. The next grant of that name carries
the tag, the fencing token of whoever set it, and its age. The server stores only the tag. The actual
result lives elsewhere.

* Release **without** a tag leaves the existing tag untouched (a holder that skips the work because
  the previous run `SUCCEEDED` does not erase that fact).
* If the lease expires, the tag becomes `ABANDONED` (server-assigned; clients cannot set it).
  `CloseSession` with grants still held also yields `ABANDONED`.
* Tags are only settable by Mutex and Write holders. Readers and semaphore holders may *read* the
  last tag on their grant but cannot set one.
* A tag has its own TTL (`state_ttl`, per release, server-capped). The name is garbage-collected
  once the tag expires.

## Events (named broadcasts)

An event is a named broadcast, like `asyncio.Event` with a limited memory. Any number of clients wait on a name.
One client sets it, which wakes every client waiting at that moment. Events are separate from locks: an event and a
lock with the same name never interact. Events have no owner and no fencing semantics.

```
EventWait{request_id, name, wait_ms, after_id}   → EventWaitOk{published | queued}
EventSet{name, value?, publish_ms?}              → EventSetOk{woken, publication_id, publish_ms}
push: EventPublished{request_id, name, publication}   (a queued wait was satisfied)
push: WaitTimedOut{request_id, ...}                   (a queued wait ran out of time)
```

### Semantics

* **Set.** A set wakes every waiter that is registered at that moment, in arrival order, and returns how many were woken
  (`woken`). Waiters that arrive later are not woken by it.
* **Publish window.** A set keeps the event *published* for `publish_ms` after the set. A wait that starts inside that
  window returns at once with the same value and `age_ms`. After the window the publication is gone and waiters wait for
  the next set.
* **Latest set wins.** A new set replaces whatever was published before, including with a zero window. `publish_ms = 0`
  wakes only the waiters present now and clears any earlier publication.
* **Window size.** An absent `publish_ms` uses `DISTLOCK_EVENT_DEFAULT_PUBLISH_MS` (10 s). A longer request is capped at
  `DISTLOCK_EVENT_MAX_PUBLISH_MS` (600 s) and the applied window is returned in `EventSetOk.publish_ms`. Oversize windows
  are capped, not rejected.
* **Value.** Optional string, at most `DISTLOCK_MAX_EVENT_VALUE_BYTES` (4096). A longer value is rejected with
  `InvalidArgument`. A set without a value wakes waiters with `value` absent.
* **Publication ids.** Each set takes one id from the server-wide counter that also issues fencing tokens, so ids
  strictly increase across all events and locks, and across restarts. A zero-window set still takes an id.
* **`after_id` filter.** A wait is satisfied by a publication only if its id is greater than `after_id`. A consumer that
  loops on one event passes the id of the last publication it handled, so it never gets the same publication twice.
  The Python `Event` and the Rust `EventWait::after` do this for the caller.
* **Waits belong to the session**, not to a connection. They share the request-id space with `Acquire`, so
  `CancelAcquire` withdraws a wait. A wait ends with the session (`CloseSession`, reaping). `SessionView.pending_events`
  lists the waits still registered, so a reconnecting client can tell whether its wait survived.
* **Idempotent wait retry.** Re-sending an `EventWait` with the same `request_id` and name returns `Waiting` instead of
  registering twice. The same `request_id` with a different name is `RequestConflict`.
* **Wait timeouts.** `wait_ms` is capped at `DISTLOCK_MAX_WAIT_MS`. `wait_ms = 0` succeeds only when the event is
  published right now, and otherwise fails at once. A queued wait that runs out gets `WaitTimedOut`.
* **Limits.** `DISTLOCK_MAX_WAITERS_PER_LOCK` applies per event name. `DISTLOCK_MAX_REQUESTS_PER_SESSION` counts event
  waits together with lock requests.
* **Set is not retried.** A set is not idempotent (each call makes a new publication). If the connection fails during a set,
  the client returns `ConnectionLost` and the outcome is unknown.
* **Delivery.** A wake-up is pushed to one connection of the waiting session, newest first. If the session has no
  connection when the set happens, that wake-up cannot be delivered. The client then waits again, and it is answered
  at once if the publish window is still open. So an event that must not be missed needs a window longer than a
  reconnect (default 10 s).
* **Garbage collection.** An event name is dropped when it has no waiters and no live publication. The name table
  therefore never grows with idle names.

### Naming on the wire

Events use the `event` family: `namespace:event:name` (or `event:name` without a namespace). An event `x` and a lock `x` never
conflict. Namespaces may not contain `:`.

### Observability

`Stats.events` counts event *names* that have waiters or a live publication. It does not count waiters, so a client cannot
use it to confirm how many waiters are registered. Adding a waiter count to `StatsOk` would make that possible (see
*Known limitations*).

### Client APIs

* **Rust:** `Client::wait_event(EventWait)` and `Client::set_event(EventSet)`, with blocking forms `wait_event` and
  `set_event` on the blocking client. `EventWait::wait(d)` caps the wait, `Some(0)` only checks the current publication,
  and a timeout returns `Error::Timeout`. `set_event` returns `SetResult{woken, ...}`.
* **Python:** `client.Event(name, namespace=, timeout=, publish_for=)` returns a reusable `Event` with `wait` / `set`
  (awaitable) and `wait_sync` / `set_sync` (blocking). An `Event` remembers the last publication its `wait` returned, so a
  loop never receives the same one twice. `last_id` exposes that id, and `reset()` forgets it. Each `Event` can be used
  from many tasks or threads.

### Testing

* Core (`crates/distlock-core/tests/events.rs`): windows, latest-set-wins, timeouts, cancel, retry, session close,
  request-id conflicts, and limits. The core property test (`properties.rs`) also generates event operations.
* Server (`crates/distlock-server/tests/events.rs`): the raw protocol. Covers validation, pushes, a wait that survives a
  reconnect, a set made while the session has no connection, and session close.
* Rust client (`crates/distlock-client/tests/events.rs`): the client API, including waits across cut connections and
  a set made while the waiter is offline.
* Python (`python/tests/test_events.py`, 16 tests): the documented usage, `after` and `last_id` loops, cancellation,
  Ctrl-C in a blocking wait, and the publish window. Most tests synchronise with `settle()`, a fixed 0.25 s sleep. The
  one test that has been changed uses `registered()`, which polls `stats().events`. See *Known limitations*.

## Architecture

```
distlock-proto   protobuf schema → prost types (compiled with protox, no protoc needed)
distlock-core    LockTable: a pure, synchronous state machine. No IO, no clock, no tokio.
distlock-server  tokio TCP front end: framing, bcrypt login, session registry, one engine task owning the LockTable
distlock-client  pooled async + blocking client: session, reconnect + reclaim, auto-renewed leases
python/          PyO3 extension over distlock-client + the pure-Python package `distlock` (own cargo workspace)
```

**Why a hand-rolled `LockTable` instead of DashMap / tokio `Semaphore`?** The grant must be owned by a
*session* and released by a *message* (from any connection), by a *timer*, or by session close. Tokio's
semaphore permits are RAII values tied to a task. We also need FIFO queues with per-waiter timeouts
and cancel, a fencing token per grant, a state tag per name, and lock-delay. All of that is a handful
of invariants over one data structure; DashMap would only add per-shard locking around state that
must change atomically. A single-owner state machine is deterministic and trivial to property-test.
The clock is an *input* (`now: Duration`), so tests are exact and fast.

One actor thread owning the whole table is far from the bottleneck (the network is). Sharding later
means N actors keyed by `hash(name)`, which is also the unit that would map to nodes.

## The server (`distlock-server`)

```
 TCP ─▶ connection task ──Command──▶ ┌────────────────────────┐
 TCP ─▶ connection task ──Command──▶ │ engine task            │──▶ per-connection outbound queue ─▶ writer task ─▶ TCP
 TCP ─▶ connection task ──Command──▶ │  LockTable + sessions  │
        (bcrypt runs here)           └────────────────────────┘
                                       sleeps until table.next_deadline()
```

* **Wire format:** `u32` big-endian length + one protobuf frame (`crates/distlock-proto/proto/distlock/v1/distlock.proto`).
  Frames over `DISTLOCK_MAX_FRAME_BYTES` are a protocol error. `seq` is non-zero on every request; `0`
  marks a server push.
* **Connection task:** enforces the handshake timeout, runs bcrypt (on the blocking pool, at most
  `DISTLOCK_AUTH_MAX_CONCURRENT_BCRYPT` at once), answers `Ping` itself, closes silent connections after
  `DISTLOCK_IDLE_CONNECTION_TIMEOUT_MS`, and forwards everything else to the engine. A failed login
  closes the connection: one attempt per connection.
* **Engine task:** the only code that touches lock state. It turns each request into a `LockTable`
  call plus a reply, sleeps until the table's next timer, and turns table events into pushes.
* **Pushes** (`Granted`, `WaitTimedOut`, `LeaseExpired`) go to *one* connection of the session, newest
  first (requirement #9). If the session has no connection the push is dropped; the client learns the
  truth from the view returned by its next `Attach`.
* **Backpressure:** requests queue into the engine on a bounded channel, so a slow engine slows readers
  (TCP backpressure). The engine never waits on a connection: if a connection's outbound queue (1024
  frames) is full, that connection is killed rather than waited for.
* **Sessions:** created by `Hello`, joined by `Attach`, ended by `CloseSession`, or reaped after
  `DISTLOCK_SESSION_IDLE_TIMEOUT_MS` with no connection, no held lock and no pending request.
* **Shutdown** (`SIGINT`/`SIGTERM`): stop accepting, push `Shutdown{drain_ms}`, wait one second, close
  every connection, exit 0. All lock state is lost, as on any restart.

* **TLS** (`crates/distlock-server/src/tls.rs`, rustls with the `ring` provider) is off unless `DISTLOCK_TLS_ENABLED=true`.
  The listener then serves TLS 1.2 and 1.3 only. The server reads the certificate chain and key once at start-up and refuses
  to start if the files are missing, hold no certificate, or the key does not match the certificate. The private key must be
  unencrypted, because the server has nobody to ask for a passphrase at start-up. Setting the cert or key file without
  the flag is an error, so a certificate is never silently ignored. Renewing a certificate needs a restart. Clients are not
  asked for certificates. A plaintext client is dropped by a TLS server, and the reverse also fails. The certificate
  scripts live in `scripts/tls/`.
* **Stats** (`Stats` request, answered by the engine in `actor.rs`): version, uptime, connections, sessions, connected
  clients, locks held, requests queued, lock names, event names, startup grace, and the MD5 of the users file in force
  (`users_md5`). It also returns system and process
  CPU, memory and file usage, sampled over a window of about 200 ms (`stats.rs`). Optional fields are absent when the
  platform cannot report them. `events` counts event names with waiters or a live publication, not individual waiters.

Running it:

```bash
cp .env.example .env
cp config/users.example.json config/users.json && chmod 600 config/users.json
scripts/hash-password.sh                       # paste the hash into config/users.json
DISTLOCK_STARTUP_GRACE_MS=0 cargo run -p distlock-server   # 0 = skip the post-restart wait in development
```

## The clients

### Rust (`distlock-client`)

Deliberately bare-bones: settings in, locks out.

```rust
let client = DistLockClient { client_id, password, host, port, namespace, pool_size, tls, default_ttl }
    .connect().await?;                                   // or .connect_blocking()?
let lock = client.acquire(Acquire::mutex("job").ttl(d).wait(d)).await?;
lock.release_with(Some(State::Succeeded), None).await?;  // or lock.release(), or just drop it
```

* One `Client` is one **session**, carried by `pool_size` TCP connections. Slot 0 logs in (`Hello`, bcrypt)
  and the session id is assigned by the server; the other slots and every reconnect `Attach` to it.
  A supervisor task per slot reconnects with jittered exponential backoff (100 ms to 5 s).
* **Reverify after reconnect:** every `Attach` returns the server's view of the session, and the client
  reconciles against it. A queued acquire whose `Granted` push was lost is completed from the view (with the
  previous holder's status, which the view carries); a held lock the view no longer lists is marked lost.
  Local state is only judged against a view that was requested *after* that state was established, so a
  view that predates a fresh grant cannot cause a false "lost".
* **Session lost** (the server restarted or reaped it): every held lock and waiter is failed, and the client logs in
  again by itself; later acquires work on the new session.
* **Leases** are renewed by a task per lock at a third of the granted lease. A lock is *lost* when the server says so
  (`LeaseExpired` push, `NotHeld` on extend), when reconciliation proves it, or when renewals keep failing past the
  locally computed expiry. `Lock::is_lost` / `wait_lost` / `watch_lost` expose it.
* **Cancel-safety:** a dropped `acquire` future withdraws its request (`CancelAcquire`), which also releases a grant
  that raced in. A `Granted` push nobody waits for is released at once. `release` runs its RPC as a separate task, so
  cancelling the caller cannot strand a half-released lock. Dropping a `Lock` releases it in the background.
* Requests retry on another connection if theirs dies (acquire and extend are idempotent through the request id;
  a retried release that finds the lock gone counts as success). `Unavailable` (the server's startup grace)
  is retried until the caller's `wait` runs out.
* **TLS.** `tls: true` makes the client use TLS with two independent checks, `tls_verify_ca` (the certificate chains to a
  trusted CA, from `ca_file_path` or `ca_file_content`) and `tls_verify_host` (the certificate names the host or IP in the
  URL). Both default to on. Turning one off gives "CA only" or "name only". Turning both off encrypts without
  authenticating the server. The client never presents a certificate of its own.
* The blocking client owns a two-thread runtime and must not be used from async code.

**Names on the wire.** The client sends `namespace:family:name`, with `family` one of `mutex`, `rw`, `sem`
(`name` alone prefixed with the family when there is no namespace). So a mutex and a read/write lock with the same
name never conflict, a read lock and a write lock do (same family), and namespaces keep applications apart.
The server treats the whole string as an opaque name; a *raw* protocol client that reuses one name for two kinds
still gets `KIND_MISMATCH`. `:` is not allowed in a namespace (it is in names).

### Keeping connections and leases alive

Three layers, because they fail in different ways:

| Layer | Catches | How |
|---|---|---|
| **Application heartbeat** (primary) | a dead *or hung* peer, a partition, idle timers in NATs and L7 proxies | The server tells each client its `ping_interval` (default 10 s). The client pings on every pool connection at half of it. The server answers from the connection task, so even a busy lock engine answers. Any frame resets the client's watchdog; a whole `ping_interval` of silence (two unanswered pings) makes the client close the connection itself and reconnect, instead of waiting for the kernel. The server mirrors this: it drops clients silent for `idle_connection_timeout` (at least twice the interval). |
| **TCP keepalive** (backstop) | a dead peer when nothing else notices; keeps NAT mappings warm | `SO_KEEPALIVE` on both ends: first probe after 30 s idle, then every 10 s, three probes. |
| **`TCP_USER_TIMEOUT`** (Linux only) | data stuck unacknowledged on a dead path | 30 s. Keepalive never fires while data waits for an ACK, so otherwise the kernel retransmits for about fifteen minutes. Not exercised on macOS, where this tree is developed. |

*Why not TCP keepalive alone:* a hung peer process is still ACKed by its kernel, so keepalive cannot see it;
the defaults are two hours; and L7 proxies and load balancers measure idleness in application bytes. *Why not
the heartbeat alone:* it is the right primary signal, but the socket options cost nothing and bound the worst
case at the kernel level. Heartbeats and lease renewals run on the client's own runtime threads, never on the
Python event loop and never under the GIL, so blocking or CPU-bound Python code cannot starve them (the Python
suite proves this with a rival thread trying to steal the lock while the holder blocks or spins for five leases).

Related client behaviour:

* **Renewal attempts are bounded.** Each attempt, including waiting for a connection, is limited to the renewal
  interval (a third of the lease) and retried on another connection, so a hung connection cannot consume a lease.
  A lock is declared lost only when the server says so, reconciliation proves it, or renewals fail past the locally
  computed expiry.
* **Flapping is backed off.** A connection that dies within 5 s of being made is retried after 100 ms, 200 ms, …
  up to 5 s (±25 % jitter) instead of at once. One that lived longer reconnects immediately.
* **Failures are explained.** If no connection can be had for 10 s the caller gets the last real reason:
  `Auth` when the server refused the credentials on re-login (a rotated password), otherwise `Connect` with the OS error.
* `Client::connections()`, `Client::reconnects()` and `Client::ping()` (round-trip time) expose pool health; the
  Python client has `connections`, `reconnects` and `ping()`.

### Python (`python/`)

* `distlock._native` (`python/src/lib.rs`, PyO3 0.30, `abi3-py39`) is a thin string-typed layer over the async Rust
  client on one shared two-thread tokio runtime. Every operation exists as an awaitable (`pyo3-async-runtimes`) and
  as a blocking call that releases the GIL and wakes every 100 ms so Ctrl-C works (the interrupted request is
  withdrawn). Failures are `NativeError(kind, message)`.
* `distlock` (pure Python) is the public API: `ClientSettings` (pydantic + `urllib` URL parsing), `DistLockClient`,
  reusable `Lock`s, per-grant `LockGuard`s, `LockMode` / `ProcessingStatus` enums, and an exception hierarchy
  (`translate` maps native kinds to it; `LockTimeout` is also a `TimeoutError`, and so on). Typed: `py.typed` plus
  `_native.pyi`.
* A `Lock` can be entered repeatedly and concurrently. Since `__aexit__` is called on the lock, not the guard, the active
  guards live in a `ContextVar`, which is per task and per thread.
* `from_url` does no I/O: the pool connects on first use, so a client can be built at import time. The session id is
  `client.session_id` once connected.
* `set_status` is recorded locally and applied by the release at the end of the block. An exception in the block still
  releases (and applies the status set before it) and is never masked by a release error; a lost lease raises `LockLost`
  on exit unless another exception is propagating.
* Default `mode` of `client.lock()` is `MUTEX` (a shared default would give no exclusion).
* Events are exposed as `client.Event(...)`, described in *Events*. `client.stats()` and `stats_sync()` return a
  `ServerStats` with the counters from the server and a `SystemStats` / `ProcessStats` pair.
* Key login: `auth_mode=v2` in the URL, with the private key as the password. `python -m distlock.keys` generates the key pair
  (needs the `keys` extra).
* TLS: `tls=true` in the URL, or the `tls`, `ca_file_path` and `ca_file_content` arguments, which take precedence over the URL.
* `min_version` (default `0.3.0`) makes the client refuse a server older than that. The check runs after login and fails with
  `ServerTooOld`.

### Operator tool (`distlock-health`)

`distlock-health` (in the `distlock-client` crate, behind the default `cli` feature) is one binary with explicit
subcommands, parsed with `clap`. Run with none, or with old-style flags and no subcommand, it prints the command
list and exits 2: nothing happens implicitly. The server URL is required by the commands that need one (`--url`,
`--url -` for one line on stdin, or `$DISTLOCK_URL`); it is no longer read from a pipe unless asked for.

| Command | Does |
|---|---|
| `check` | Logs in afresh and reports the server's memory, CPU, open files, load, connected clients, locks held, startup-grace state and `users_md5`, as `key: value` lines. `--interval N` repeats it forever. Each report is POSTed as JSON to the OK or the error webhook (`--ok-webhook` / `--error-webhook`, or `DISTLOCK_OK_WEBHOOK` / `DISTLOCK_ERROR_WEBHOOK`; an empty variable means none). `--human-readable` prints sizes as `16.4 GiB`, percentages as `24.3%` and integers as `92_160` in the text only; webhook JSON keeps plain numbers. |
| `reset-users` | Sends `Maintenance{ResetUsers}` and prints the counts and the `users_md5` now in force. |
| `tls new-ca`, `tls use-ca`, `tls server-cert` | The certificates for the server's TLS listener, with `rcgen` and `x509-parser` instead of `openssl`. |
| `keys generate` | An Ed25519 key pair for `auth_mode=2`, with `ring`. |

Exit codes: `0` success, `1` the action failed, `2` bad usage, `3` (`check --require-ready`) the server is in its startup
grace period. The connection flags (`--tls`, `--ca-file`, `--no-verify-ca`, `--no-verify-host`, `--timeout`) are shared by
`check` and `reset-users`. The same figures are available from the Python `client.stats()`.

**TLS tooling.** Keys are ECDSA P-256 in PKCS#8 PEM, which the server reads. `new-ca` writes a self-signed CA
(`basicConstraints CA:TRUE`, `keyCertSign`); `server-cert` signs a leaf with `serverAuth`, the requested DNS and IP
names in the subjectAltName, a random 16-byte serial, and validity starting five minutes ago so a slightly slow client
clock does not see it as "not yet valid". `use-ca` accepts an existing CA only if it is a CA, has not expired, and its
key (when given) matches the certificate. An intermediate CA's certificate is appended after the leaf. With a CA
certificate but no CA key, `server-cert` writes a CSR instead. Files are created owner-only for keys, and nothing is
replaced without `--force`. Keys with a passphrase are refused (the tool cannot ask for one, nor can the server); the
shell scripts in `scripts/tls/` still handle them. This tool adds about 1 MB to the release binary (clap, rcgen,
x509-parser), measured at 3.5 MB to 4.4 MB stripped on macOS arm64.

## Reloading the users

The users live in an immutable snapshot behind an `ArcSwap` (`crates/distlock-server/src/users.rs`). A login check
loads the current snapshot (lock-free, never blocked by a writer) and uses it from start to finish, so it sees one
complete set of users, never a half-applied update. A reload builds the next snapshot entirely off to the side
(read, validate, hash a new dummy hash only if the highest bcrypt cost changed) and publishes it with one atomic
pointer swap, so readers pay nothing for it. The only lock is a mutex that orders reloads against *each other*;
no reader touches it. A reload that fails (unreadable or invalid file) publishes nothing, so the previous users
stay in force and checks keep working. Re-reading an unchanged file is one file read, no hashing, no swap.

Triggered by **`client.maintenance.reset_users()`** (Rust: `client.maintenance().reset_users()`, CLI:
`distlock-health reset-users`), wire message
`Maintenance{ResetUsers}`. The connection task that received it does the reload (never the lock engine, so a slow
reload cannot stall anyone's locks) and hands the diff to the engine:

| In the new file | Effect |
|---|---|
| user **removed**, or newly **disabled** | thrown out: the engine ends their sessions (locks released with tag `ABANDONED`, after the usual lock-delay, because the revoked process may still be mid-critical-section; waiting requests cancelled) and closes **every one of their TCP connections** |
| user **added**, or re-**enabled** | can log in from now on |
| **password changed** | applies to new logins only; live sessions are left alone |

If the caller itself is thrown out, it still gets its report: the engine replies *first*, and the close is queued
behind the reply on that connection (`invoker_dropped` tells it). Afterwards its pool cannot log in again, so
its calls raise `AuthenticationError`, and it keeps retrying in the background, so it recovers by itself if the user
is added back.

Two races are closed in the engine, which re-checks the lock-free snapshot whenever a session is created or joined:
a login whose password was verified just before a swap is refused if the user is gone by the time the engine sees it,
and a session whose user lost access cannot be joined. (Both have deterministic unit tests that drive the engine
directly, since a socket cannot hit the window reliably.)

**The users file hash.** The snapshot also holds the MD5 of the bytes it was parsed from (`users_md5`, 32 lowercase hex
characters; empty for users built in memory). It is computed once when a snapshot is built, so reporting it reads no
disk, and it always describes the users in force: a reload that fails publishes nothing, and the hash stays. The server
logs it at start-up (`users loaded`) and on every reset (`users reset requested`, or `users reloaded` when something
changed). It is returned in `ResetUsersOk.users_md5` and `StatsOk.users_md5`, so `distlock-health reset-users` and
`check` (and the Python `report.users_md5` / `stats.users_md5`) can show it. Its purpose is to confirm that every
server runs the file that was rolled out. MD5 is only a change detector here: it is not collision-resistant and
authenticates nothing. A server older than this field returns an empty string. There is one server in v1; across
several, each server's file is its own, so compare each server's hash with the hash of the file you put there.

**Authorization.** There is no per-user permission yet, so *any logged-in client may send this command*. That is
bounded on purpose: the command only makes the server re-read a file an administrator controls (a client cannot
inject users), the effect is the diff of that file, an unchanged file costs one file read, reloads are serialized,
the reply carries counts only (no names, so it cannot be used to enumerate other applications), and the error for an
unloadable file is generic (paths and names stay in the server log). When RBAC arrives, this is the first command to
put behind a permission. Editing `users.json` alone changes nothing until a reload is requested; a `SIGHUP` trigger
would reuse the same path.

## Configuration

| File | Purpose | In git? |
|---|---|---|
| [`.env.example`](.env.example) → `.env` | Server settings as environment variables: network, auth/session, lock limits, logging. Shows every default; contains no secrets. | example only |
| [`config/users.example.json`](config/users.example.json) → `config/users.json` | Who may log in: `client_id` + bcrypt hash. | example only |
| [`config/users.schema.json`](config/users.schema.json) | JSON Schema for the users file. Editors pick it up through the file's `$schema` key. | yes |
| [`scripts/hash-password.sh`](scripts/hash-password.sh) | Makes the bcrypt hash for a user (stdin or no-echo prompt; never argv). | yes |

Setting up credentials:

```bash
cp .env.example .env
cp config/users.example.json config/users.json && chmod 600 config/users.json
scripts/hash-password.sh      # prompts twice; paste the printed hash into "password_hash"
```

The example hashes belong to random passwords that were discarded, so copying the example file
unmodified gives nobody a way in. Hand each client team its own random password
(`openssl rand -base64 33`), and store only the hash.

**Users file rules.** The schema covers shape; the loader (`crates/distlock-server/src/users.rs`) also enforces:

* `client_id` is both the login name and the application name on every session. Unique and
  case-sensitive; a duplicate is a startup error.
* `password_hash` is bcrypt (`$2a$`/`$2b$`/`$2y$`), cost ≥ 10. bcrypt only reads the first 72 bytes of
  a password, so use random passwords of at most 72 bytes.
* `public_keys` (optional) lists Ed25519 keys for auth mode 2, each with `key` (43 base64 characters, URL-safe or standard),
  `identifier` (written to the log when the key logs someone in), `issuer` (metadata, not read) and an optional RFC 3339
  `expiry`. A user needs a `password_hash`, one or more keys, or both. An expired key stops new logins but leaves open
  sessions alone. A wrong key, an unknown or disabled user and an expired key all get the same `invalid credentials` error.
* `enabled: false` rejects new logins without deleting the entry. Unknown fields are rejected, except
  the human-only `$schema` and `description`.
* An unreadable or invalid file is a startup error. A group/world-readable file should at least warn.
* Reload with `client.maintenance.reset_users()` or `distlock-health reset-users` (see *Reloading the users*): removing or disabling a client terminates its
  live sessions. Nothing is re-read until a reload is requested.

**Event and auth variables.** `DISTLOCK_EVENT_DEFAULT_PUBLISH_MS` (10 s), `DISTLOCK_EVENT_MAX_PUBLISH_MS` (600 s; the default
must not exceed it) and `DISTLOCK_MAX_EVENT_VALUE_BYTES` (4096; must not exceed half the frame limit) bound events.
`DISTLOCK_MIN_AUTH_MODE` (1 by default; 2 refuses passwords) selects the accepted login modes. The TLS variables are
`DISTLOCK_TLS_ENABLED`, `DISTLOCK_TLS_CERT_FILE` and `DISTLOCK_TLS_KEY_FILE`, described under *The server*. Each is parsed
and range-checked at start-up, and an invalid value stops the server with the variable name in the error.

**Environment.** The lock-limit variables map 1:1 onto `distlock_core::Config`, with the same defaults,
except `DISTLOCK_STARTUP_GRACE_MS`: the core defaults to 0 but the server defaults it to `MAX_TTL_MS` (see
*Known limitations → Restart*). `token_floor` is deliberately *not* configurable: the server seeds it
from the wall clock at startup. The `DISTLOCK_AUTH_MAX_CONCURRENT_BCRYPT`, `*_MAX_CONNECTIONS*`,
`*_TIMEOUT_MS` and `*_PING_INTERVAL_MS` knobs are server concerns the core never sees. The trailing
client-side block (`DISTLOCK_ADDR`, `DISTLOCK_CLIENT_ID`, `DISTLOCK_PASSWORD`, …) is for the client
library's examples and CLI; those belong in the client application's environment, not the server's.

## Testing

* Unit tests for every semantic above.
* Key login: `crates/distlock-server/tests/keyauth.rs` covers a valid signature, a signature replayed on another
  connection, a signature for another user name, unknown modes and out-of-order messages, and `DISTLOCK_MIN_AUTH_MODE=2`.
  `python/tests/test_keyauth.py` covers a key login, refusal of a wrong key, an unknown user and an expired key, and key generation.
* TLS: `crates/distlock-client/tests/tls.rs` covers each combination of the two verification switches, a client that does not
  trust the CA, TLS against plaintext, and the server refusing to start with an unusable certificate. `python/tests/test_tls.py`
  covers the same behaviour from Python, plus the certificate scripts.
* `check_invariants()` on the table: mutual exclusion per kind, no missed wakeups, index ↔ queue
  consistency, no past-due leases, every lease/waiter has a timer, no idle residue.
* Property test: random op sequences with the invariant checked after every step, and a
  "drain everything" leak check at the end (no entries, sessions or timers remain).
* Server end-to-end tests (`crates/distlock-server/tests/e2e.rs`): a real server on a loopback socket
  and a raw protobuf client. They cover auth, protocol violations, timeouts, limits, pools, reconnect +
  reclaim, idempotent retry, lease expiry + lock-delay, `max_hold`, `CloseSession`, shutdown.
  Timing assertions are lower bounds only, so a slow machine slows tests down but does not flake them.
* Resilience tests (`crates/distlock-client/tests/resilience.rs`) put a TCP proxy that can cut one connection, cut all,
  black-hole traffic without closing anything, refuse connections, or flap in front of the real server, and check:
  idle connections stay open far past the server's idle timeout (heartbeat; fails if pings are disabled), a partition is
  noticed within one ping interval, a 2 s lease survives a partition, the pool refills after losing one or all connections
  with the same session, a flapping server is not hammered (8 attempts in 3 s instead of 40), and a rotated password or a
  dead server is reported with its real reason.
* Client tests (`crates/distlock-client/tests`): the real server in-process, plus a TCP proxy that can be cut or paused to
  prove reconnect and reclaim: locks surviving a dropped connection, a grant made while disconnected, a lease that expired
  while away, a server restart; plus cancellation, drop-releases, contention, namespaces/families, statuses, blocking API.
* Python tests (`python/tests`, 142 collected): URL parsing and validation (no server), then the whole API against the real
  `distlock-server` binary through async and blocking calls, including cancellation, Ctrl-C, lost leases and a server
  restart. CI runs them on Python 3.11 and 3.13 (macOS and Linux). `test_resilience.py` proves leases keep renewing while the
  event loop is blocked, while Python spins holding the GIL, and after the client object is garbage-collected (a rival thread
  tries to take the lock throughout), and that idle connections survive a 600 ms server idle timeout. Disabling the renewal
  fails all four renewal tests.
* Users reload: unit tests on the `ArcSwap` snapshot (the diff, no-op, failure keeps the old users, and, with a verifier
  running on every worker thread while the file is swapped 150 times, that **no check ever fails**; readers keep running
  during a slow reload), engine-level tests of the two login-race guards, end-to-end tests of revocation over real TCP
  (pool dropped, locks freed with lock-delay and `ABANDONED`, invoker removing itself, broken file, password change),
  and the same through the Rust and Python clients. The core's property test includes forced session revokes.
* The server's `.env.example` and `config/users.example.json` are parsed by unit tests, so the shipped
  examples cannot drift from the loaders.
* Planned: protocol decoder fuzzing, a turmoil/madsim simulation of reconnect + reclaim.

## Known limitations / accepted for v1

* A *stale duplicate* `Acquire` arriving after its request was granted **and released** would
  enqueue again. Needs a per-session completed-request cache; unlikely across a single ordered stream.
* Removing a timed-out waiter scans the queue: O(n) in queue length (bounded by `max_waiters_per_lock`).
* **Restart:** all state is lost. The server must refuse acquires for one `max_ttl` after start
  (`startup_grace`) so leases granted by the previous incarnation drain. Clients see `SESSION_NOT_FOUND`
  on `Attach` and must treat every lock as lost.
* Wall-clock going backwards across a restart could weaken `token_floor`.
* TLS is off by default. Without it, passwords, session tokens and lock traffic are visible on the wire. Use it, or keep
  the server on a private network. Key login does not protect the session token, so it also needs TLS.
* **Events have no delivery guarantee across a lost session.** A wake-up is dropped if no connection is attached when the
  set happens, and only a still-open publication window can recover it. A set with a zero window made while the waiter is
  offline is missed (tested). Choose the window to be longer than a reconnect.
* **Events have no waiter count in `Stats`.** `events` counts names, so a client or test cannot confirm how many waiters
  are registered. The Python test suite waits with a fixed `settle()` sleep, which can be too short on a slow machine. The
  one test changed so far polls `events`, which is weaker. Adding `waiters` to `StatsOk` would close this gap.
* **Event wait queues are bounded per name** by `DISTLOCK_MAX_WAITERS_PER_LOCK`. A full queue refuses the wait with
  `ResourceExhausted`.
* **Login brute force** is only throttled indirectly: bcrypt cost, the concurrent-verification cap and
  one attempt per connection. There is no per-IP or per-`client_id` lockout yet.
* The `AttachOk` session view is one frame. A session with thousands of outstanding requests under
  long lock names could exceed `max_frame_bytes`; the write then fails and the connection closes.
  The default limits (10 000 requests × 256-byte names) allow this in theory, not in practice.
* A client that stops reading is disconnected once 1024 frames are queued for it, and its locks stay
  held until their leases expire. Clients must read continuously and `Attach` again on loss.
* `users.json` is re-read only when a client sends `ResetUsers` (there is no file watcher and no `SIGHUP` trigger yet).
* `ResetUsers` has no authorization beyond being logged in (see *Reloading the users*).
* `users_md5` is MD5 of the file's bytes, so a change that only reformats the file (whitespace, key order) changes the hash
  and counts as a reload even though no user changed. It detects change, not meaning, and is not a security measure.
* `distlock-health tls` cannot read or write passphrase-protected keys, and does not make a CA key with a passphrase
  (`new-ca.sh --encrypt-key`). `scripts/tls/` still does. Windows is built in CI but the owner-only key file mode is
  applied only on Unix; on Windows the key file gets the folder's default permissions.
* A changed password does not end live sessions; to cut a leaked credential off completely, remove or disable the user.
* The client renews each lease from its own task, one RPC per lock per renewal. `Extend` is batched on the wire, but the
  client does not batch yet; thousands of simultaneously held locks would be better served by one renewer per client.
* The client discovers a lost lease through a push, a failed renewal, or reconnect reconciliation; there is no way to
  interrupt code that is mid-block. Python raises `LockLost` on exit; `guard.check()` is for the steps in between.
* The Python `guard.set_status()` is applied at release. If the process dies before then, the status is `ABANDONED`.
* A client dropped without `close()` leaves its session (and held locks) until their leases expire.
* `pool_size` must stay below the server's `DISTLOCK_MAX_CONNECTIONS_PER_SESSION` (default 32). A replaced half-open connection
  still occupies a slot on the server until its idle timeout, so a pool sized at the limit is refused (and retried with backoff)
  for up to that long after a partition.
* `TCP_USER_TIMEOUT` is Linux-only and compiled but not exercised here.
* Verified on macOS only. On the external `/Volumes` disk used in development, `cargo clippy` sometimes
  fails with spurious "can't find crate" errors in parallel builds; `CARGO_TARGET_DIR` on a local disk
  avoids it. Builds and tests are unaffected. Likewise, `maturin develop` fails to copy the built library across volumes
  (extended attributes are refused there); building into the project's own `target/` avoids it.

## Next steps

1. Client: batch lease renewals per client; typed Python stubs generated from the build; a published wheel pipeline.
2. `AcquireMany`: server-side canonical (sorted) order with all-or-nothing rollback on timeout,
   built on top of single-lock acquire.
3. Admin: list holders / force release, Prometheus metrics, failed-auth rate limiting, a `SIGHUP` trigger for the reload.
4. Later: persistence, sharding, RBAC, client certificates (mTLS).
5. Events: a waiter count in `StatsOk`, so tests can wait for registration without a fixed sleep.
