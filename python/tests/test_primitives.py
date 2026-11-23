"""Primitives: client-less definitions, bound again and again, to one client or to several."""

from __future__ import annotations

import asyncio

import pytest

import distlock
from distlock import (
    ConfigError,
    EventPrimitive,
    InvalidArgument,
    Lock,
    LockBusy,
    LockMode,
    LockTimeout,
    MutexPrimitive,
    ProcessingStatus,
    ReadLockPrimitive,
    SemaphorePrimitive,
    WriteLockPrimitive,
)


def run(coro):
    return asyncio.run(coro)


def test_one_primitive_binds_repeatedly_to_the_same_client(server, name):
    async def main():
        client = server.client()
        job = MutexPrimitive(name, ttl=5)
        tokens = []
        for _ in range(3):
            async with job.bind(client) as guard:
                assert guard.name == name
                assert guard.mode is LockMode.MUTEX
                assert guard.ttl == 5
                tokens.append(guard.fencing_token)
        assert tokens == sorted(set(tokens)), "each bind is a fresh grant"
        await client.close()

    run(main())


def test_one_primitive_binds_to_different_clients(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        job = MutexPrimitive(name)
        async with job.bind(a) as guard:
            assert await job.bind(b).try_acquire() is None
            with pytest.raises(LockTimeout):
                await job.bind(b).acquire(wait=0.1)
            guard.set_status(ProcessingStatus.SUCCEEDED)
        async with job.bind(b) as guard:
            assert guard.status is ProcessingStatus.SUCCEEDED
        await a.close()
        await b.close()

    run(main())


def test_client_attach_is_the_same_as_bind(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        job = WriteLockPrimitive(name)
        async with a.attach(job) as guard:
            assert isinstance(a.attach(job), Lock)
            with pytest.raises(LockBusy):
                await b.attach(job).acquire(wait=0)
            assert guard.mode is LockMode.WRITE
        async with b.attach(job):
            pass
        await a.close()
        await b.close()

    run(main())


def test_attach_works_in_blocking_code_too(server, name):
    a, b = server.client("billing"), server.client("reports")
    job = MutexPrimitive(name)
    with a.attach(job):
        assert job.bind(b).try_acquire_sync() is None
    with job.bind(b) as guard:
        assert guard.name == name
    a.close_sync()
    b.close_sync()


def test_read_write_and_mutex_primitives_keep_their_semantics(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        readers, writers = ReadLockPrimitive(name), WriteLockPrimitive(name)
        async with a.attach(readers), b.attach(readers):  # readers share
            assert await b.attach(writers).try_acquire() is None
        async with a.attach(writers):
            assert await b.attach(readers).try_acquire() is None
            async with b.attach(MutexPrimitive(name)):  # a separate family
                pass
        await a.close()
        await b.close()

    run(main())


def test_semaphore_primitive_counts_permits(server, name):
    async def main():
        client = server.client()
        sem = SemaphorePrimitive(name, capacity=2)
        async with client.attach(sem), client.attach(sem):
            assert await client.attach(sem).try_acquire() is None
        guard = await client.attach(sem).try_acquire()
        assert guard is not None
        await guard.release()
        await client.close()

    run(main())


def test_namespace_and_wait_belong_to_the_primitive(server, name):
    async def main():
        client = server.client()
        in_x = MutexPrimitive(name, namespace="x")
        in_y = MutexPrimitive(name, namespace="y")
        async with client.attach(in_x):
            assert await client.attach(in_y).try_acquire() is not None
            quick = MutexPrimitive(name, namespace="x", wait=0.1)
            with pytest.raises(LockTimeout):
                await client.attach(quick).acquire()
        await client.close()

    run(main())


def test_event_primitive_binds_a_fresh_event_each_time(server, name):
    async def main():
        a, b = server.client("billing"), server.client("reports")
        prim = EventPrimitive(name, timeout=5, publish_for=1)
        waiter = asyncio.create_task(prim.bind(b).wait())
        await asyncio.sleep(0.3)
        assert await a.attach(prim).set("go") == 1
        assert await waiter == "go"
        assert prim.bind(a) is not prim.bind(a)
        assert prim.bind(a).timeout == 5 and prim.bind(a).publish_for == 1
        await a.close()
        await b.close()

    run(main())


def test_primitives_are_validated_when_made_and_when_attached():
    with pytest.raises(InvalidArgument):
        MutexPrimitive("")
    with pytest.raises(InvalidArgument):
        ReadLockPrimitive("x", ttl=0)
    with pytest.raises(InvalidArgument):
        WriteLockPrimitive("x", namespace="a:b")
    with pytest.raises(InvalidArgument):
        MutexPrimitive("x", wait=-1)
    with pytest.raises(ConfigError):
        SemaphorePrimitive("x", capacity=0)
    with pytest.raises(ConfigError):
        SemaphorePrimitive("x", capacity=2, permits=3)
    with pytest.raises(InvalidArgument):
        EventPrimitive("x", timeout=-1)
    with pytest.raises(TypeError):
        MutexPrimitive("x").bind(object())


def test_attach_rejects_what_is_not_a_primitive(server):
    client = server.client()
    with pytest.raises(InvalidArgument):
        client.attach("not-a-primitive")  # type: ignore[arg-type]


def test_the_exported_names_exist():
    for exported in (
        "Primitive",
        "MutexPrimitive",
        "ReadLockPrimitive",
        "WriteLockPrimitive",
        "SemaphorePrimitive",
        "EventPrimitive",
        "defaults",
    ):
        assert exported in distlock.__all__
        assert hasattr(distlock, exported)
