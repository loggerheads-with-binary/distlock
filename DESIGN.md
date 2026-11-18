# distlock — design (v1: single node, in-memory)

A distributed lock service written in Rust. Mutexes, RwLocks and semaphores, leased with TTLs,
spoken over TCP with a protobuf wire protocol.

## Scope of v1

| In | Out (deliberately deferred) |
|---|---|
| Single node, all state in memory | Multi-node / sharding (but the design keeps the door open) |
| bcrypt username/password auth | TLS / mTLS (private network only for now) |
| Authentication only | RBAC / per-lock authorization |
| Protobuf over length-prefixed TCP | Persistence (WAL / snapshots) |
| Mutex, RwLock, semaphore | Re-entrancy (we do *reclaim*, see below), RwLock upgrade/downgrade |

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

* **Wire format:** `u32` big-endian length + one protobuf frame (`proto/distlock/v1/distlock.proto`).
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
* `tls: true` is rejected (`Error::TlsUnsupported`): the server has no TLS yet.
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

## Reloading the users

The users live in an immutable snapshot behind an `ArcSwap` (`crates/distlock-server/src/users.rs`). A login check
loads the current snapshot (lock-free, never blocked by a writer) and uses it from start to finish, so it sees one
complete set of users, never a half-applied update. A reload builds the next snapshot entirely off to the side
(read, validate, hash a new dummy hash only if the highest bcrypt cost changed) and publishes it with one atomic
pointer swap, so readers pay nothing for it. The only lock is a mutex that orders reloads against *each other*;
no reader touches it. A reload that fails (unreadable or invalid file) publishes nothing, so the previous users
stay in force and checks keep working. Re-reading an unchanged file is one file read, no hashing, no swap.

Triggered by **`client.maintenance.reset_users()`** (Rust: `client.maintenance().reset_users()`), wire message
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
* `enabled: false` rejects new logins without deleting the entry. Unknown fields are rejected, except
  the human-only `$schema` and `description`.
* An unreadable or invalid file is a startup error. A group/world-readable file should at least warn.
* Reload with `client.maintenance.reset_users()` (see *Reloading the users*): removing or disabling a client terminates its
  live sessions. Nothing is re-read until a reload is requested.

**Environment.** The lock-limit variables map 1:1 onto `distlock_core::Config`, with the same defaults,
except `DISTLOCK_STARTUP_GRACE_MS`: the core defaults to 0 but the server defaults it to `MAX_TTL_MS` (see
*Known limitations → Restart*). `token_floor` is deliberately *not* configurable: the server seeds it
from the wall clock at startup. The `DISTLOCK_AUTH_MAX_CONCURRENT_BCRYPT`, `*_MAX_CONNECTIONS*`,
`*_TIMEOUT_MS` and `*_PING_INTERVAL_MS` knobs are server concerns the core never sees. The trailing
client-side block (`DISTLOCK_ADDR`, `DISTLOCK_CLIENT_ID`, `DISTLOCK_PASSWORD`, …) is for the client
library's examples and CLI; those belong in the client application's environment, not the server's.

## Testing

* Unit tests for every semantic above.
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
* Python tests (`python/tests`, 93): URL parsing and validation (no server), then the whole API against the real
  `distlock-server` binary through async and blocking calls, including cancellation, Ctrl-C, lost leases and a server
  restart. Run on Python 3.14 and, from the installed wheel, 3.9. `test_resilience.py` proves leases keep renewing while the
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
* No TLS: credentials and traffic are visible on the wire. Private network only.
* **Login brute force** is only throttled indirectly: bcrypt cost, the concurrent-verification cap and
  one attempt per connection. There is no per-IP or per-`client_id` lockout yet.
* The `AttachOk` session view is one frame. A session with thousands of outstanding requests under
  long lock names could exceed `max_frame_bytes`; the write then fails and the connection closes.
  The default limits (10 000 requests × 256-byte names) allow this in theory, not in practice.
* A client that stops reading is disconnected once 1024 frames are queued for it, and its locks stay
  held until their leases expire. Clients must read continuously and `Attach` again on loss.
* `users.json` is re-read only when a client sends `ResetUsers` (there is no file watcher and no `SIGHUP` trigger yet).
* `ResetUsers` has no authorization beyond being logged in (see *Reloading the users*).
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
4. Later: persistence, sharding, RBAC, TLS.
