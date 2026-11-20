"""Keep-alive behaviour as seen from Python.

* A held lock's lease is renewed by the Rust side, on its own threads, so it keeps being renewed
  however busy or blocked the Python side is (a blocked event loop, a CPU-bound loop holding the GIL).
* The pool's heartbeats keep idle connections open, and the pool reconnects by itself.
"""

from __future__ import annotations

import asyncio
import gc
import threading
import time

import pytest

from distlock import (
    AuthenticationError,
    ClientClosed,
    ConfigError,
    ConnectionFailed,
    DistLockClient,
    InvalidArgument,
    LockBusy,
    LockLost,
    LockTimeout,
    MaintenanceError,
    ServerError,
    SessionLost,
    _native,  # noqa: F401  (imported to build NativeError instances below)
)
from distlock.errors import translate


class Rival(threading.Thread):
    """Keeps trying to take a lock from a *different* client, from its own thread, and records
    whether it ever succeeded. If a lease ever lapsed, this thread would get the lock."""

    def __init__(self, client: DistLockClient, name: str, every: float = 0.1, kind: str = "mutex") -> None:
        super().__init__(daemon=True)
        self._lock = getattr(client, kind)(name)  # must be the same kind of lock as the holder's
        self._every = every
        self._stop_event = threading.Event()
        self.attempts = 0
        self.successes = 0
        self.error = None

    def run(self) -> None:
        try:
            while not self._stop_event.is_set():
                guard = self._lock.try_acquire_sync()
                self.attempts += 1
                if guard is not None:
                    self.successes += 1
                    guard.release_sync()
                time.sleep(self._every)
        except BaseException as e:  # noqa: BLE001 - reported by the test
            self.error = e

    def finish(self) -> None:
        self._stop_event.set()
        self.join(10)


def wait_for(what: str, condition, within: float = 10.0) -> None:
    deadline = time.monotonic() + within
    while not condition():
        assert time.monotonic() < deadline, f"gave up waiting for: {what}"
        time.sleep(0.05)


# ------------------------------------------------------------------------------ lease renewal


def test_the_lease_is_renewed_while_the_event_loop_is_blocked(server, name):
    async def main():
        holder, rival_client = server.client("billing"), server.client("reports")
        rival = Rival(rival_client, name)
        async with holder.mutex(name, ttl=0.5) as guard:
            rival.start()
            wait_for("the rival to be running", lambda: rival.attempts >= 2)
            time.sleep(2.5)  # blocks the event loop AND this thread: five lease lengths
            assert not guard.lost
            rival.finish()
        assert rival.error is None
        assert rival.attempts >= 10, "the rival must have had many chances to take the lock"
        assert rival.successes == 0, "the lease lapsed while Python was blocked"

    asyncio.run(main())


def test_the_lease_is_renewed_while_python_holds_the_gil_computing(server, name):
    holder, rival_client = server.client("billing"), server.client("reports")
    rival = Rival(rival_client, name)
    with holder.mutex(name, ttl=0.5) as guard:
        rival.start()
        wait_for("the rival to be running", lambda: rival.attempts >= 2)
        deadline = time.monotonic() + 2.5
        spins = 0
        while time.monotonic() < deadline:  # CPU-bound pure Python, no waiting anywhere
            spins += 1
        assert spins > 1000
        assert not guard.lost
        rival.finish()
    assert rival.error is None and rival.attempts >= 10
    assert rival.successes == 0, "the lease lapsed while Python was busy"


def test_a_lock_is_still_renewed_after_the_client_object_goes_out_of_scope(server, name):
    def acquire_and_forget_the_client():
        return server.client("billing").mutex(name, ttl=0.4).acquire_sync()

    guard = acquire_and_forget_the_client()  # the DistLockClient itself is unreachable from here on
    gc.collect()
    rival = Rival(server.client("reports"), name)
    rival.start()
    time.sleep(1.8)  # four and a half lease lengths
    rival.finish()
    assert not guard.lost
    assert rival.successes == 0
    guard.release_sync()


def test_a_long_hold_across_many_leases_never_loses_the_lock(server, name):
    holder, rival_client = server.client("billing"), server.client("reports")
    rival = Rival(rival_client, name, every=0.05, kind="wlock")
    with holder.wlock(name, ttl=0.6) as guard:
        rival.start()
        for _ in range(8):
            time.sleep(0.5)  # four seconds, nearly seven lease lengths, in small steps
            guard.check()
        rival.finish()
    assert rival.attempts >= 20 and rival.successes == 0


# ------------------------------------------------------------------------------ heartbeat and the pool


def test_heartbeats_keep_idle_connections_open_and_the_ping_reports_latency(make_server):
    srv = make_server(DISTLOCK_PING_INTERVAL_MS="200", DISTLOCK_IDLE_CONNECTION_TIMEOUT_MS="600")
    client = DistLockClient.from_url(srv.url("billing", pool_size=3))
    assert client.connections == 0 and client.reconnects == 0  # nothing happens before first use
    client.connect_sync()
    wait_for("the pool to come up", lambda: client.connections == 3)
    session = client.session_id

    time.sleep(3.0)  # five server idle timeouts, with no requests at all

    assert client.connections == 3
    assert client.reconnects == 0, "heartbeats should have kept every connection up"
    assert client.session_id == session
    assert 0 < client.ping_sync() < 0.5
    assert asyncio.run(client.ping()) >= 0


def test_the_pool_reconnects_by_itself_after_a_server_restart(make_server, name):
    srv = make_server()
    client = srv.client(pool_size=3)
    client.connect_sync()
    wait_for("the pool to come up", lambda: client.connections == 3)
    old_session = client.session_id

    srv.restart()  # a brand-new server that has never heard of this session

    wait_for("every connection to be re-established", lambda: client.reconnects >= 3 and client.connections == 3, 30)
    assert client.session_id != old_session, "the client should have logged in again"
    with client.mutex(name) as guard:
        assert not guard.lost


# ------------------------------------------------------------------------------ errors


@pytest.mark.parametrize(
    "kind, expected",
    [
        ("config", ConfigError),
        ("invalid_argument", InvalidArgument),
        ("auth", AuthenticationError),
        ("connect", ConnectionFailed),
        ("not_connected", ConnectionFailed),
        ("connection_lost", ConnectionFailed),
        ("unavailable", ConnectionFailed),
        ("session_lost", SessionLost),
        ("timeout", LockTimeout),
        ("busy", LockBusy),
        ("lease_lost", LockLost),
        ("closed", ClientClosed),
        ("maintenance", MaintenanceError),
        ("server", ServerError),
        ("protocol", ServerError),
        ("internal", ServerError),
        ("a-kind-from-the-future", ServerError),
    ],
)
def test_every_native_error_kind_maps_to_a_public_exception(kind, expected):
    error = translate(_native.NativeError(kind, "boom"))
    assert type(error) is expected and "boom" in str(error)
