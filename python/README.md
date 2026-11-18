# distlock (Python)

Distributed locks for Python: mutexes, read/write locks and semaphores, with leases that renew
themselves, on top of the Rust client in `crates/distlock-client`. Async and blocking, one API.

```python
from distlock import DistLockClient, ProcessingStatus

client = DistLockClient.from_url(
    "distlock://billing:s3cret@10.0.0.5:7878/billing?pool_size=5&default_ttl=30"
)

async with client.mutex("nightly-report") as guard:
    if guard.status is ProcessingStatus.SUCCEEDED:
        return                                   # the previous holder already did the work
    ...                                          # the lease is renewed for you while this runs
    guard.set_status(ProcessingStatus.SUCCEEDED) # recorded when the block ends
```

## Install and build

The package is a PyO3 extension (`distlock._native`, built with [maturin](https://www.maturin.rs))
plus a pure-Python layer. One `abi3` wheel serves Python 3.9 and newer.

```bash
cd python
python -m venv .venv && source .venv/bin/activate
pip install maturin pytest "pydantic>=2.5"
maturin develop                    # builds the Rust code and installs the package, editable
cargo build -p distlock-server     # the tests start the real server (from the repo root: ../target/debug)
pytest
```

`maturin build --release` makes a wheel. Only `pydantic` is needed at runtime. The tests need
`htpasswd` (macOS has it; `apache2-utils` elsewhere) to create a users file with the repo's
`scripts/hash-password.sh`; set `DISTLOCK_SERVER_BIN` to use a server binary from elsewhere.

## Connecting

```python
client = DistLockClient.from_url("distlock://client_id:password@host:port/namespace?pool_size=5&tls=false&default_ttl=30")
client = DistLockClient.from_env()                       # reads $DISTLOCK_URL
client = DistLockClient(client_id="billing", password="s3cret", host="10.0.0.5", namespace="billing")
```

| URL part | Meaning | Default |
|---|---|---|
| `client_id:password` | a user from the server's users file (percent-encode `@ : / %`) | required |
| `host`, `port` | the server | port `7878` |
| `/namespace` | prefix for every lock name (`namespace:...`), to keep applications apart | none |
| `pool_size` | TCP connections (1–256); they all share one session | `4` |
| `default_ttl` | lease length in seconds for locks that do not set `ttl` | `30` |
| `tls` | not supported by the server yet; `true` raises `ConfigError` | `false` |

URLs are parsed with `urllib` and validated with pydantic ([`ClientSettings`](distlock/settings.py));
mistakes raise `ConfigError` with a message that never contains the password.

Creating a client does no I/O. The pool connects the first time a lock is acquired (or on
`await client.connect()`, to fail fast on bad credentials). The server assigns a **session id**
when the pool is set up, visible as `client.session_id`; all connections of the pool share it, so any
connection can carry any request, and a dropped connection is re-established and the session
re-joined without losing locks.

## Locks

```python
lock = client.lock("name", ttl=None, namespace=None, mode=LockMode.WRITE, wait=None)

client.rlock("name")      # shared: many readers at once, never alongside a writer
client.wlock("name")      # exclusive: blocks and is blocked by every reader and writer of that name
client.mutex("name")      # exclusive, in its own space: a mutex "x" and an rlock/wlock "x" do not conflict
client.semaphore("name", capacity=5, permits=1)
```

* `ttl` is the lease length in seconds (default: the client's `default_ttl`). It is **renewed in the
  background** for as long as you hold the lock, so it is a liveness bound, not a time limit on your work.
* `wait` is how long acquiring may wait: `None` (default) as long as the server allows, `0` not at all,
  otherwise `LockTimeout` after that many seconds.
* A `Lock` is a reusable definition. Enter it as many times as you like, from as many tasks or threads
  as you like; each entry gets its own grant and its own `LockGuard`.
* Read and write locks are fair (FIFO): a waiting writer is not starved by later readers.
* A semaphore's first acquirer defines its capacity; it disappears when its last holder and waiter are gone.

```python
async with lock as guard: ...                 # or:  with lock as guard: ...   (blocking)
guard = await lock.acquire(wait=5)            # raises LockTimeout; guard.release() when done
guard = await lock.try_acquire()              # None if taken right now
guard = lock.acquire_sync() / lock.try_acquire_sync()
```

Cancellation is safe: cancelling a waiting `acquire` (a `wait_for` timeout, a cancelled task) withdraws
the request from the server, so it can never be granted to nobody; Ctrl-C interrupts a blocking acquire the same way.

## The guard

| | |
|---|---|
| `guard.status` | the previous holder's `ProcessingStatus` (`SUCCEEDED`, `FAILED`, `STORED`, or `ABANDONED`), else `None` |
| `guard.previous` | the same with `set_by_token` and `age` |
| `guard.set_status(status, ttl=None)` | leave your outcome for the next holder; applied when the block ends |
| `guard.fencing_token` | strictly increasing across all grants; hand it to what you protect and reject older ones |
| `guard.lost` / `guard.check()` / `await guard.wait_lost()` | has the lease been lost? |
| `await guard.release()` / `guard.release_sync()` | release early (idempotent) |
| `await guard.extend(ttl)` | lengthen the lease beyond what renewal gives you (rarely needed) |

Statuses: only **mutex and write** holders can set one (`InvalidArgument` otherwise); readers and
semaphore holders can read it. Releasing without `set_status` leaves the previous status untouched.
`ABANDONED` is assigned by the server when a holder's lease expired, or its session ended, while it
still held the lock; you can read it but not set it. A status stays readable for the server's
`state_ttl` (10 minutes by default; `set_status(..., ttl=...)` overrides it).

If the lease is lost while you hold the lock (the server restarted, a long network partition), `guard.lost`
turns true, and leaving the block raises `LockLost` (unless another exception is already propagating, which
is never masked). Call `guard.check()` before irreversible steps of a long job.

## Errors

Everything derives from `DistLockError`. Where a standard exception fits, it is also that:
`LockTimeout` is a `TimeoutError`, `ConnectionFailed` a `ConnectionError`, `InvalidArgument`/`ConfigError` a `ValueError`.

| exception | when |
|---|---|
| `ConfigError` | bad URL or settings |
| `AuthenticationError` | wrong client id / password |
| `ConnectionFailed` | server unreachable or not ready (the pool keeps retrying in the background) |
| `LockTimeout` / `LockBusy` | waited out / `wait=0` and it was taken |
| `LockLost` | the lease expired before release |
| `MaintenanceError` | a maintenance command could not be carried out; nothing changed |
| `SessionLost` | the server forgot the session; the client logs in again by itself, but old locks are gone |
| `ClientClosed` | the client was closed |
| `InvalidArgument` / `ServerError` | rejected argument / anything else the server reports |

## Staying alive

* **Leases renew themselves** for as long as a guard is held: every `ttl / 3` the Rust client sends a lease extension, on its
  own threads. That is independent of Python, so a blocked event loop (`time.sleep` in async code), a CPU-bound loop holding the
  GIL, or a client object that went out of scope cannot make a lease lapse. Each renewal attempt is bounded and retried on
  another connection, so one hung connection cannot eat a lease either.
* **Heartbeats keep the pool's connections up.** Every connection pings the server at half the server's `ping_interval`
  (default 10 s). A connection that hears nothing for a full interval is considered dead and replaced, and TCP keepalive
  backs this up at the OS level.
* **The pool repairs itself.** Dropped connections are re-established with jittered backoff, rejoining the same session (your
  locks are re-verified against the server's view); if the server forgot the session (a restart), the client logs in again and
  any locks held through the old session report `lost`.

```python
client.connections       # pool connections up right now
client.reconnects        # connections re-established since the start
await client.ping()      # round-trip time to the server, in seconds (client.ping_sync() when blocking)
```

If every connection is down for more than ten seconds, calls raise `AuthenticationError` when the server refuses the credentials
on re-login (a rotated password), and `ConnectionFailed` (with the underlying reason) otherwise.

## Maintenance

```python
report = await client.maintenance.reset_users()      # or client.maintenance.reset_users_sync()
```

Tells the server to re-read its users file (`DISTLOCK_USERS_FILE`) and swap it in. Logins are never refused or delayed while this
happens (the server swaps an immutable snapshot atomically). Users that are **gone from the file or now disabled are thrown
out**: their sessions end, their locks are released (the next holder sees `ABANDONED`, after the server's lock-delay) and the
server closes **every one of their TCP connections**, including yours if you removed yourself: you still get the report, with
`report.invoker_dropped` set, and after that your calls raise `AuthenticationError` until the user is added back, which this
client then notices by itself. Users added or re-enabled can log in; a changed password applies to new logins only.

| `ResetUsersReport` | |
|---|---|
| `added`, `removed`, `disabled`, `enabled`, `password_changed` | counts of users in each category (names are in the server log, not in the reply) |
| `sessions_closed`, `connections_closed` | what was thrown out |
| `invoker_dropped` | you were among them |
| `unchanged` | nothing at all changed |

If the file cannot be loaded you get `MaintenanceError`, nothing changes, and the old users stay in force. Calling it again is
always safe. There is no authorization beyond being logged in yet, so any client can send it; it can only make the server
re-read a file the administrator controls.

## Lifecycle

```python
async with DistLockClient.from_url(url) as client:   # connects now; on exit ends the session
    ...                                              # (the server releases every lock it still holds)
client.close_sync()                                  # or: await client.close()
```

Dropping the last reference to a guard releases its lock (without a status); a client that is simply
dropped stops its pool and leaves its session to expire on the server.

## Names on the wire

The server sees `namespace:family:name` with `family` one of `mutex`, `rw`, `sem`. That is how a mutex
and a read/write lock with the same name stay separate, and how namespaces keep applications apart.
`:` is therefore not allowed in a namespace (it is fine inside a lock name).
