"""The Python API against a real server: async and sync, semantics, failure handling."""

from __future__ import annotations

import _thread
import asyncio
import threading
import time

import pytest

import distlock
from distlock import (
    AuthenticationError,
    ClientClosed,
    ConnectionFailed,
    DistLockClient,
    InvalidArgument,
    LockBusy,
    LockLost,
    LockMode,
    LockTimeout,
    ProcessingStatus,
    ServerError,
)


def run(coro):
    return asyncio.run(coro)


# ------------------------------------------------------------------------------ the basics


def test_the_documented_usage_pattern_works_and_a_lock_instance_is_reusable(server, name):
    async def main():
        client = server.client()
        lock = client.Lock(name, ttl=None, namespace=None, mode=LockMode.WRITE)

        async with lock as _guard:
            first = _guard.fencing_token
            assert _guard.name == name and _guard.status is None and not _guard.lost
            assert _guard.ttl == 30.0  # the client's default_ttl

        async with lock as _guard:  # the same instance, entered again
            assert _guard.fencing_token > first

        assert client.session_id is not None  # assigned when the pool was set up

    run(main())


def test_a_mutex_excludes_and_the_second_caller_waits_for_release(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        order = []

        async def contender():
            async with b.Mutex(name) as _:
                order.append("b")

        async with a.Mutex(name) as _:
            task = asyncio.create_task(contender())
            await asyncio.sleep(0.3)
            assert not task.done(), "must be waiting while a holds the lock"
            order.append("a")
        await asyncio.wait_for(task, 5)
        assert order == ["a", "b"]

    run(main())


def test_try_acquire_wait_zero_and_timeouts(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        async with a.Mutex(name) as _:
            lock = b.Mutex(name)
            assert await lock.try_acquire() is None
            with pytest.raises(LockBusy):
                await lock.acquire(wait=0)
            started = time.monotonic()
            with pytest.raises(LockTimeout) as info:
                await lock.acquire(wait=0.25)
            assert time.monotonic() - started >= 0.2
            assert isinstance(info.value, TimeoutError)  # also a builtin TimeoutError
        guard = await lock.try_acquire()
        assert guard is not None
        await guard.release()

    run(main())


def test_one_lock_instance_shared_by_many_tasks_is_mutually_exclusive(server, name):
    async def main():
        client = server.client(pool_size=4)
        lock = client.Mutex(name)
        counter = {"n": 0}

        async def worker():
            for _ in range(5):
                async with lock:
                    seen = counter["n"]
                    await asyncio.sleep(0)  # a point where an unprotected update would be lost
                    counter["n"] = seen + 1

        await asyncio.gather(*(worker() for _ in range(8)))
        assert counter["n"] == 40

    run(main())


def test_nested_locks_and_early_release_inside_the_block(server, name):
    async def main():
        client = server.client()
        outer, inner = client.Mutex(name + "-a"), client.Mutex(name + "-b")
        async with outer as g_outer:
            async with inner as g_inner:
                assert g_inner.fencing_token > g_outer.fencing_token
            await g_outer.release()  # explicit, early
            assert g_outer.released
            assert await client.Mutex(name + "-a").try_acquire() is not None
        # leaving the block after an explicit release is fine

    run(main())


# ------------------------------------------------------------------------------ families and namespaces


def test_rlock_and_wlock_are_two_sides_of_one_lock_and_mutexes_are_separate(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        async with a.RLock(name) as _r1, b.RLock(name) as _r2:  # readers share
            assert await b.WLock(name).try_acquire() is None  # a writer cannot join readers
            m = await b.Mutex(name).try_acquire()  # a mutex of the same name is unrelated
            assert m is not None
            await m.release()
        async with a.WLock(name) as _w:
            assert await b.RLock(name).try_acquire() is None
            assert await b.WLock(name).try_acquire() is None
            m = await b.Mutex(name).try_acquire()
            assert m is not None, "a mutex must not conflict with a write lock of the same name"
            await m.release()

    run(main())


def test_namespaces_isolate_applications_and_can_be_overridden_per_lock(server, name):
    async def main():
        a = DistLockClient.from_url(server.url("billing") + "/app-a")
        b = DistLockClient.from_url(server.url("reports") + "/app-b")
        async with a.Mutex(name) as _:
            assert await b.Mutex(name).try_acquire() is not None  # different namespace: no conflict
            assert await b.Mutex(name, namespace="app-a").try_acquire() is None  # override: same lock as a's
            other = await b.Mutex(name, namespace="").try_acquire()  # "" = no namespace at all
            assert other is not None

    run(main())


def test_semaphores_limit_holders_and_report_a_capacity_mismatch(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        sem = lambda client: client.Semaphore(name, capacity=2)  # noqa: E731
        async with sem(a) as _1, sem(b) as _2:
            assert await sem(a).try_acquire() is None
            with pytest.raises(ServerError, match="(?i)capacity"):
                await a.Semaphore(name, capacity=3).try_acquire()
        guard = await sem(a).try_acquire()
        assert guard is not None
        with pytest.raises(InvalidArgument):
            guard.set_status(ProcessingStatus.SUCCEEDED)  # semaphores cannot leave a status
        await guard.release()

    run(main())


# ------------------------------------------------------------------------------ status of the last holder


def test_the_previous_holders_status_reaches_the_next_holder(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        async with a.Mutex(name) as first:
            assert first.status is None and first.previous is None
            first.set_status(ProcessingStatus.STORED)
            first_token = first.fencing_token
        async with b.Mutex(name) as second:
            assert second.status is ProcessingStatus.STORED
            assert second.previous.set_by_token == first_token and second.previous.age >= 0
            # no set_status here: the old one must survive
        async with a.Mutex(name) as third:
            assert third.status is ProcessingStatus.STORED
            third.set_status("failed", ttl=60)  # plain strings and a custom lifetime are fine
        async with b.Mutex(name) as fourth:
            assert fourth.status is ProcessingStatus.FAILED

    run(main())


def test_the_example_from_the_spec(server, name):
    async def main():
        async with server.client().Mutex(name) as guard:
            status = guard.status
            assert status is None
            guard.set_status(ProcessingStatus.STORED)
        async with server.client("reports").Mutex(name) as guard:
            assert guard.status == ProcessingStatus.STORED

    run(main())


def test_a_write_lock_can_leave_a_status_but_a_read_lock_cannot(server, name):
    async def main():
        client = server.client()
        async with client.WLock(name) as w:
            w.set_status(ProcessingStatus.SUCCEEDED)
        async with client.RLock(name) as r:
            assert r.status is ProcessingStatus.SUCCEEDED  # readers see the last writer's status
            with pytest.raises(InvalidArgument):
                r.set_status(ProcessingStatus.FAILED)
        async with client.Mutex(name + "-m") as m:
            with pytest.raises(InvalidArgument):
                m.set_status(ProcessingStatus.ABANDONED)  # only the server assigns that one
            with pytest.raises(ValueError):
                m.set_status("nonsense")

    run(main())


def test_a_holder_that_vanishes_leaves_abandoned(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        held = await a.Mutex(name).acquire()  # never released (and kept alive, so not auto-released)...
        await a.close()  # ...its session ends while it still holds the lock
        async with b.Mutex(name) as guard:
            assert guard.status is ProcessingStatus.ABANDONED
        assert held.lost or not held.released

    run(main())


def test_an_exception_in_the_block_releases_the_lock_and_propagates_unchanged(server, name):
    async def main():
        client = server.client()

        class Boom(Exception):
            pass

        with pytest.raises(Boom):
            async with client.Mutex(name) as guard:
                guard.set_status(ProcessingStatus.FAILED)
                raise Boom
        async with client.Mutex(name) as again:
            assert again.status is ProcessingStatus.FAILED  # the status set before the error was applied

    run(main())


# ------------------------------------------------------------------------------ leases


def test_the_lease_is_renewed_while_the_block_runs(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        async with a.Mutex(name, ttl=0.4) as guard:
            await asyncio.sleep(1.6)  # four times the lease
            assert not guard.lost
            assert await b.Mutex(name).try_acquire() is None
            guard.check()  # does not raise

    run(main())


def test_a_lost_lease_is_flagged_and_reported_on_exit(make_server, name):
    srv = make_server(DISTLOCK_MAX_HOLD_MS="800")  # the server ends any lease after 0.8 s, renewals or not

    async def main():
        client = srv.client()
        with pytest.raises(LockLost):
            async with client.Mutex(name, ttl=0.4) as guard:
                await asyncio.wait_for(guard.wait_lost(), 5)
                assert guard.lost
                with pytest.raises(LockLost):
                    guard.check()
        # An exception already propagating is not masked by the lost lease.
        with pytest.raises(KeyError):
            async with client.Mutex(name, ttl=0.4) as guard:
                await asyncio.wait_for(guard.wait_lost(), 5)
                raise KeyError("the real problem")

    run(main())


def test_a_server_restart_loses_the_locks_and_the_client_recovers_by_itself(make_server, name):
    srv = make_server()

    async def main():
        client = srv.client()
        guard = await client.Mutex(name).acquire()
        old_session = client.session_id
        srv.restart()
        await asyncio.wait_for(guard.wait_lost(), 30)
        with pytest.raises(LockLost):
            await guard.release()
        async with client.Mutex(name) as fresh:  # a new session, transparently
            assert not fresh.lost
        assert client.session_id != old_session

    run(main())


# ------------------------------------------------------------------------------ cancellation, errors, lifecycle


def test_cancelling_an_acquire_leaves_no_phantom_waiter(server, name):
    async def main():
        a, b, c = server.client("billing"), server.client("reports"), server.client("billing")
        holder = await a.Mutex(name).acquire()
        with pytest.raises(asyncio.TimeoutError):
            await asyncio.wait_for(b.Mutex(name).acquire(), 0.4)  # cancelled while queued
        await asyncio.sleep(0.2)  # let the cancel reach the server
        await holder.release()
        guard = await c.Mutex(name).try_acquire()  # would be None had b's request stayed queued
        assert guard is not None

    run(main())


def test_cancelling_a_task_inside_the_block_still_releases_the_lock(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")

        async def holder():
            async with a.Mutex(name) as _:
                await asyncio.sleep(60)

        task = asyncio.create_task(holder())
        await asyncio.sleep(0.3)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        guard = await asyncio.wait_for(b.Mutex(name).acquire(wait=3), 5)
        await guard.release()

    run(main())


def test_bad_credentials_and_unreachable_servers_raise_specific_errors(server, name):
    async def main():
        bad = DistLockClient.from_url(server.url("billing", password="wrong"))
        with pytest.raises(AuthenticationError):
            await bad.connect()
        with pytest.raises(AuthenticationError):
            await bad.Mutex(name).acquire()
        nobody = DistLockClient.from_url("distlock://billing:x@127.0.0.1:1")
        with pytest.raises(ConnectionFailed):
            await nobody.Mutex(name).acquire()

    run(main())


def test_closing_the_client_releases_everything_and_blocks_further_use(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        await a.Mutex(name).acquire()
        await a.WLock(name).acquire()
        await a.close()
        assert await b.Mutex(name).try_acquire() is not None
        assert await b.WLock(name).try_acquire() is not None
        with pytest.raises(ClientClosed):
            await a.Mutex(name + "x").acquire()

    run(main())


def test_the_client_is_an_async_and_a_sync_context_manager(server, name):
    async def main():
        async with DistLockClient.from_url(server.url("billing")) as client:
            assert client.session_id is not None
            await client.Mutex(name).acquire()
        # leaving the block ended the session, so the lock is free
        async with server.client("reports").Mutex(name) as _:
            pass

    run(main())
    with DistLockClient.from_url(server.url("billing")) as client:
        assert client.session_id is not None
        client.Mutex(name + "-s").acquire_sync()
    guard = server.client("reports").Mutex(name + "-s").try_acquire_sync()
    assert guard is not None
    guard.release_sync()


# ------------------------------------------------------------------------------ the blocking API


def test_the_blocking_api(server, name):
    a, b = server.client("billing"), server.client("reports")
    with a.Mutex(name) as guard:
        guard.set_status(ProcessingStatus.SUCCEEDED)
        assert b.Mutex(name).try_acquire_sync() is None
        with pytest.raises(LockTimeout):
            b.Mutex(name).acquire_sync(wait=0.2)
    with b.Mutex(name) as guard:
        assert guard.status is ProcessingStatus.SUCCEEDED
    guard = a.WLock(name + "-w").acquire_sync()
    assert guard.extend_sync(5.0) > 0
    guard.release_sync()
    guard.release_sync()  # idempotent


def test_threads_sharing_a_lock_instance_are_mutually_exclusive(server, name):
    client = server.client(pool_size=4)
    lock = client.Mutex(name)
    counter = {"n": 0}

    def worker():
        for _ in range(5):
            with lock:
                seen = counter["n"]
                time.sleep(0.001)
                counter["n"] = seen + 1

    threads = [threading.Thread(target=worker) for _ in range(6)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(30)
    assert counter["n"] == 30


def test_ctrl_c_interrupts_a_blocking_acquire_and_withdraws_the_request(server, name):
    holder_client, waiter = server.client("billing"), server.client("reports")
    holder = holder_client.Mutex(name).acquire_sync()
    interrupter = threading.Timer(0.5, _thread.interrupt_main)
    started = time.monotonic()
    interrupter.start()
    try:
        with pytest.raises(KeyboardInterrupt):
            waiter.Mutex(name).acquire_sync()  # would wait for minutes
    finally:
        interrupter.cancel()
    assert time.monotonic() - started < 5
    time.sleep(0.3)  # let the cancel reach the server
    holder.release_sync()
    guard = holder_client.Mutex(name).try_acquire_sync()
    assert guard is not None, "the interrupted waiter must not have stayed in the queue"
    guard.release_sync()


def test_a_dropped_guard_releases_its_lock(server, name):
    a, b = server.client("billing"), server.client("reports")
    a.Mutex(name).acquire_sync()  # the guard is garbage-collected at once
    guard = b.Mutex(name).acquire_sync(wait=5)
    guard.release_sync()


def test_the_package_exposes_its_version_and_public_names():
    assert distlock.__version__
    for public in distlock.__all__:
        assert hasattr(distlock, public), public
