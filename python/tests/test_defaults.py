"""``distlock.defaults``: process-wide default ttl / wait values and how they cascade."""

from __future__ import annotations

import asyncio
import time

import pytest

from distlock import (
    InvalidArgument,
    LockBusy,
    LockTimeout,
    MutexPrimitive,
    SemaphorePrimitive,
    WriteLockPrimitive,
    defaults,
)
from distlock.defaults import (
    reset_all,
    reset_default_semaphore_wait,
    set_default_event_publish_for,
    set_default_event_wait,
    set_default_mutex_wait,
    set_default_semaphore_wait,
    set_default_ttl,
    set_default_wait,
    set_default_write_ttl,
)


@pytest.fixture(autouse=True)
def clean_defaults():
    reset_all()
    yield
    reset_all()


def run(coro):
    return asyncio.run(coro)


# ------------------------------------------------------------------------------ the resolver alone


def test_nothing_set_keeps_the_old_behaviour():
    assert defaults.resolve_wait("mutex") is None  # as long as the server allows
    assert defaults.resolve_ttl("mutex") is None  # the client's own default_ttl
    assert defaults.resolve_event_wait() is None
    assert defaults.resolve_event_publish_for() is None


def test_the_kind_default_beats_the_general_one_and_none_stops_the_cascade():
    set_default_wait(5)
    assert defaults.resolve_wait("semaphore") == 5
    set_default_semaphore_wait(9)
    assert defaults.resolve_wait("semaphore") == 9
    assert defaults.resolve_wait("mutex") == 5
    set_default_semaphore_wait(None)  # forever, on purpose: does not fall through to 5
    assert defaults.resolve_wait("semaphore") is None
    reset_default_semaphore_wait()
    assert defaults.resolve_wait("semaphore") == 5


def test_an_explicit_value_beats_every_default_and_none_means_forever():
    set_default_wait(5)
    set_default_mutex_wait(7)
    assert defaults.resolve_wait("mutex", 1) == 1
    assert defaults.resolve_wait("mutex", 0) == 0
    assert defaults.resolve_wait("mutex", None) is None


def test_ttl_cascades_and_an_explicit_none_means_not_given():
    set_default_ttl(20)
    set_default_write_ttl(11)
    assert defaults.resolve_ttl("mutex") == 20
    assert defaults.resolve_ttl("write") == 11
    assert defaults.resolve_ttl("write", 3) == 3
    assert defaults.resolve_ttl("write", None) == 11


def test_events_fall_back_to_the_general_wait():
    set_default_wait(7)
    assert defaults.resolve_event_wait() == 7
    set_default_event_wait(3)
    assert defaults.resolve_event_wait() == 3
    assert defaults.resolve_event_wait(None) is None
    set_default_event_publish_for(40)
    assert defaults.resolve_event_publish_for() == 40
    assert defaults.resolve_event_publish_for(1) == 1


def test_module_values_are_visible_at_the_top_of_the_module():
    set_default_wait(2)
    assert defaults.DEFAULT_WAIT == 2
    reset_all()
    assert repr(defaults.DEFAULT_WAIT) == "UNSET"


def test_bad_values_are_rejected():
    for bad in (0, -1, float("nan"), True, "5", 5_000_000):
        with pytest.raises(InvalidArgument):
            set_default_ttl(bad)  # type: ignore[arg-type]
    for bad in (-1, float("nan"), True, "5"):
        with pytest.raises(InvalidArgument):
            set_default_wait(bad)  # type: ignore[arg-type]
    with pytest.raises(InvalidArgument):
        set_default_event_publish_for(-1)
    with pytest.raises(InvalidArgument):
        defaults.resolve_wait("nonsense")
    assert defaults.DEFAULT_WAIT is defaults._UNSET  # nothing was stored


# ------------------------------------------------------------------------------ against a server


def test_default_wait_applies_to_locks_made_before_it_was_set(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        job = MutexPrimitive(name).bind(b)
        lock = b.Mutex(name)
        async with a.Mutex(name):
            set_default_wait(0.15)
            started = time.monotonic()
            with pytest.raises(LockTimeout):
                await job.acquire()
            assert time.monotonic() - started >= 0.15
            assert job.wait == 0.15 and lock.wait == 0.15
            set_default_wait(0)
            with pytest.raises(LockBusy):
                await lock.acquire()
        await a.close()
        await b.close()

    run(main())


def test_an_explicit_wait_beats_the_default(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        async with a.Mutex(name):
            set_default_wait(0)
            set_default_mutex_wait(0)
            started = time.monotonic()
            with pytest.raises(LockTimeout):
                await b.Mutex(name, wait=0.2).acquire()
            assert time.monotonic() - started >= 0.2
            with pytest.raises(LockTimeout):
                await b.Mutex(name).acquire(wait=0.2)
        await a.close()
        await b.close()

    run(main())


def test_wait_none_means_forever_even_when_a_default_is_set(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        async with a.Mutex(name):
            set_default_wait(0.05)
            forever = asyncio.create_task(b.Mutex(name, wait=None).acquire())
            await asyncio.sleep(0.4)
            assert not forever.done(), "still waiting long after the 0.05 s default"
            forever.cancel()
        await a.close()
        await b.close()

    run(main())


def test_the_semaphore_default_beats_the_general_default(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        set_default_wait(0)
        set_default_semaphore_wait(0.25)
        async with a.Mutex(name), a.Semaphore(name, 1):
            with pytest.raises(LockBusy):
                await b.Mutex(name).acquire()
            started = time.monotonic()
            with pytest.raises(LockTimeout):
                await SemaphorePrimitive(name, 1).bind(b).acquire()
            assert time.monotonic() - started >= 0.25
        await a.close()
        await b.close()

    run(main())


def test_default_ttl_cascades_by_kind_and_falls_back_to_the_client(server, name):
    async def main():
        client = server.client()  # the client's own default_ttl is 30 s
        async with client.Mutex(name) as guard:
            assert guard.ttl == 30
        set_default_ttl(20)
        set_default_write_ttl(11)
        async with client.Mutex(name) as m, WriteLockPrimitive(name).bind(client) as w:
            assert (m.ttl, w.ttl) == (20, 11)
        async with client.Mutex(name, ttl=9) as own:
            assert own.ttl == 9
        await client.close()

    run(main())


def test_event_defaults_apply_to_wait_and_set(server, name):
    async def main():
        client = server.client()
        set_default_wait(0.1)
        started = time.monotonic()
        with pytest.raises(LockTimeout):
            await client.Event(name).wait()
        assert time.monotonic() - started >= 0.1
        set_default_event_wait(0)
        with pytest.raises(LockTimeout):
            await client.Event(name).wait()
        assert await client.Event(name, timeout=None).set("x") == 0  # nobody was waiting

        set_default_event_publish_for(5)
        await client.Event(name).set("kept")
        assert await client.Event(name).wait() == "kept"  # still published: the default window
        reset_all()
        await client.close()

    run(main())


def test_blocking_locks_follow_the_defaults_too(server, name):
    a, b = server.client("billing"), server.client("reports")
    with a.Mutex(name):
        set_default_mutex_wait(0)
        with pytest.raises(LockBusy):
            b.Mutex(name).acquire_sync()
        set_default_mutex_wait(0.1)
        with pytest.raises(LockTimeout):
            b.Mutex(name).acquire_sync()
    a.close_sync()
    b.close_sync()
