"""Events (named broadcasts) through the Python API, against a real server."""

from __future__ import annotations

import _thread
import asyncio
import threading
import time

import pytest

import distlock
from distlock import Event, InvalidArgument, LockMode, LockTimeout


def run(coro):
    return asyncio.run(coro)


async def settle(seconds: float = 0.25) -> None:
    """Give waiters time to reach the server before the test sets the event."""
    await asyncio.sleep(seconds)


# ------------------------------------------------------------------------------ the basics


def test_the_documented_usage_wakes_every_waiter_and_set_counts_them(server, name):
    async def main():
        waiters_a = server.client()
        waiters_b = server.client("reports")
        setter = server.client()

        async def wait(client):
            return await client.event(name).wait(timeout=10)

        tasks = [asyncio.create_task(wait(c)) for c in (waiters_a, waiters_b, waiters_a)]
        await settle()

        woken = await setter.event(name).set("v1.2.3", publish_for=0)
        assert woken == 3  # == number of clients expected to be waiting
        assert await asyncio.gather(*tasks) == ["v1.2.3"] * 3

    run(main())


def test_a_set_without_a_value_gives_none_and_an_empty_string_stays_empty(server, name):
    async def main():
        client = server.client()
        await client.event(name + "-none").set()
        await client.event(name + "-empty").set("")
        assert await client.event(name + "-none").wait(timeout=1) is None
        assert await client.event(name + "-empty").wait(timeout=1) == ""

    run(main())


def test_Event_with_a_capital_letter_is_the_same_factory(server, name):
    client = server.client()
    assert isinstance(client.Event(name), Event)
    assert isinstance(client.event(name), Event)
    assert client.Event(name).name == name


def test_only_waiters_of_that_name_and_namespace_are_woken(server, name):
    async def main():
        client = server.client()
        a = asyncio.create_task(client.event(name, namespace="one").wait(timeout=5))
        b = asyncio.create_task(client.event(name, namespace="two").wait(timeout=0.8))
        await settle()
        assert await client.event(name, namespace="one").set("x", publish_for=0) == 1
        assert await a == "x"
        with pytest.raises(LockTimeout):
            await b

        # An event and a lock with the same name do not interact.
        async with client.lock(name, mode=LockMode.MUTEX):
            assert await client.event(name).set(publish_for=0) == 0

    run(main())


# ------------------------------------------------------------------------------ the window


def test_a_late_waiter_inside_the_window_is_answered_at_once_and_one_after_it_is_not(server, name):
    async def main():
        client = server.client()
        assert await client.event(name).set("kept", publish_for=0.6) == 0

        started = time.monotonic()
        assert await client.event(name).wait(timeout=5) == "kept"
        assert time.monotonic() - started < 0.4

        await asyncio.sleep(0.7)
        with pytest.raises(LockTimeout) as caught:
            await client.event(name).wait(timeout=0.3)
        assert isinstance(caught.value, TimeoutError)  # the stdlib exception, too

    run(main())


def test_a_zero_timeout_only_checks_whether_the_event_is_published(server, name):
    async def main():
        client = server.client()
        with pytest.raises(LockTimeout):
            await client.event(name).wait(timeout=0)
        await client.event(name).set("x", publish_for=5)
        assert await client.event(name).wait(timeout=0) == "x"

    run(main())


def test_the_default_publish_window_comes_from_the_server(make_server, name):
    server = make_server(DISTLOCK_EVENT_DEFAULT_PUBLISH_MS="500")

    async def main():
        client = server.client()
        await client.event(name).set("brief")  # no window given: the server's 0.5 s
        assert await client.event(name).wait(timeout=0) == "brief"
        await asyncio.sleep(0.8)
        with pytest.raises(LockTimeout):
            await client.event(name).wait(timeout=0)

    run(main())


def test_the_window_can_be_a_default_of_the_event_object(server, name):
    async def main():
        client = server.client()
        event = client.event(name, publish_for=0)
        await event.set("blink")  # window 0: nobody to wake, nothing kept
        with pytest.raises(LockTimeout):
            await client.event(name).wait(timeout=0)
        await event.set("blink", publish_for=5)  # a per-call value wins
        assert await client.event(name).wait(timeout=0) == "blink"

    run(main())


# ------------------------------------------------------------------------------ looping


def test_an_event_object_remembers_what_it_returned_so_a_loop_does_not_spin(server, name):
    async def main():
        client = server.client()
        setter = server.client("reports")
        await setter.event(name).set("1", publish_for=30)

        event = client.event(name)
        assert event.last_id == 0
        assert await event.wait(timeout=1) == "1"
        assert event.last_id > 0

        # "1" is still inside its window, but this object has handled it: it must wait for "2".
        with pytest.raises(LockTimeout):
            await event.wait(timeout=0.3)
        waiter = asyncio.create_task(event.wait(timeout=10))
        await settle()
        assert not waiter.done()
        assert await setter.event(name).set("2", publish_for=30) == 1
        assert await waiter == "2"
        # ... and having now handled "2", it must not be handed "2" again.
        with pytest.raises(LockTimeout):
            await event.wait(timeout=0.3)

        # A fresh object, or reset(), sees the current publication again.
        assert await client.event(name).wait(timeout=1) == "2"
        event.reset()
        assert event.last_id == 0
        assert await event.wait(timeout=1) == "2"

    run(main())


def test_after_can_be_given_explicitly(server, name):
    async def main():
        client = server.client()
        await client.event(name).set("a", publish_for=30)
        probe = client.event(name)
        await probe.wait(timeout=1)
        first = probe.last_id

        with pytest.raises(LockTimeout):
            await client.event(name).wait(timeout=0.3, after=first)
        assert await client.event(name).wait(timeout=1, after=first - 1) == "a"

    run(main())


# ------------------------------------------------------------------------------ cancellation


def test_cancelling_a_waiting_task_withdraws_the_wait_from_the_server(server, name):
    async def main():
        client = server.client()
        stays = asyncio.create_task(client.event(name).wait(timeout=10))
        goes = asyncio.create_task(client.event(name).wait(timeout=10))
        await settle()
        goes.cancel()
        with pytest.raises(asyncio.CancelledError):
            await goes
        await settle()

        assert await client.event(name).set(publish_for=0) == 1  # only the one still waiting
        await stays

    run(main())


def test_ctrl_c_in_a_blocking_wait_raises_promptly_and_withdraws_the_wait(server, name):
    client = server.client()
    timer = threading.Timer(0.5, _thread.interrupt_main)
    started = time.monotonic()
    timer.start()
    try:
        with pytest.raises(KeyboardInterrupt):
            client.event(name).wait_sync(timeout=30)
    finally:
        timer.cancel()
    assert time.monotonic() - started < 5
    time.sleep(0.3)
    assert client.event(name).set_sync(publish_for=0) == 0


# ------------------------------------------------------------------------------ blocking API


def test_the_blocking_api_works_from_threads(server, name):
    results = []

    def waiter(user):
        client = server.client(user)
        results.append(client.event(name).wait_sync(timeout=10))

    threads = [threading.Thread(target=waiter, args=(u,)) for u in ("billing", "reports")]
    for t in threads:
        t.start()
    time.sleep(0.6)
    assert server.client().event(name).set_sync("go", publish_for=0) == 2
    for t in threads:
        t.join(10)
    assert results == ["go", "go"]

    with pytest.raises(LockTimeout):
        server.client().event(name).wait_sync(timeout=0.2)

    # The blocking form remembers what it returned, too.
    client = server.client()
    client.event(name).set_sync("kept", publish_for=30)
    event = client.event(name)
    assert event.wait_sync(timeout=1) == "kept"
    with pytest.raises(LockTimeout):
        event.wait_sync(timeout=0.2)


# ------------------------------------------------------------------------------ validation


def test_invalid_arguments_are_rejected_with_invalid_argument(server, name):
    client = server.client()
    with pytest.raises(InvalidArgument):
        client.event("")
    with pytest.raises(InvalidArgument):
        client.event(name, namespace="bad:ns")
    with pytest.raises(InvalidArgument):
        client.event(name, timeout=-1)
    with pytest.raises(InvalidArgument):
        client.event(name, publish_for=-1)
    with pytest.raises(InvalidArgument):
        client.event(name, publish_for=True)

    async def main():
        event = client.event(name)
        with pytest.raises(InvalidArgument):
            await event.set(b"bytes")
        with pytest.raises(InvalidArgument):
            await event.wait(timeout="soon")
        with pytest.raises(InvalidArgument):
            await event.wait(after=-1)

    run(main())


def test_a_value_longer_than_the_servers_limit_is_refused(make_server, name):
    server = make_server(DISTLOCK_MAX_EVENT_VALUE_BYTES="8")

    async def main():
        event = server.client().event(name)
        with pytest.raises(InvalidArgument):
            await event.set("123456789")
        assert await event.set("12345678", publish_for=0) == 0

    run(main())


def test_the_public_api_exports_event():
    assert distlock.Event is Event
    assert "Event" in distlock.__all__
