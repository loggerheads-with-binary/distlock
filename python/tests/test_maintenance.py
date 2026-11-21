"""``client.maintenance.reset_users()``: the server reloads its users file and throws out whoever
lost access, including (if need be) the caller."""

from __future__ import annotations

import asyncio

import pytest

from distlock import (
    AuthenticationError,
    DistLockClient,
    MaintenanceError,
    ProcessingStatus,
    ResetUsersReport,
)

BOTH = {"billing": True, "reports": True}


def test_resetting_with_nothing_changed_is_a_no_op_and_nobody_is_disturbed(make_server, name):
    srv = make_server(users=BOTH)

    async def main():
        a, b = srv.client("billing"), srv.client("reports")
        async with a.Mutex(name) as guard:
            report = await b.maintenance.reset_users()
            assert isinstance(report, ResetUsersReport) and report.unchanged, report
            assert not guard.lost
            assert await b.Mutex(name).try_acquire() is None

    asyncio.run(main())


def test_added_users_can_log_in_afterwards(make_server, name):
    srv = make_server(users=BOTH)
    reports = srv.client("reports")
    with pytest.raises(AuthenticationError):
        srv.client("newcomer").Mutex(name).acquire_sync()

    srv.write_users({**BOTH, "newcomer": True})
    report = reports.maintenance.reset_users_sync()
    assert (report.added, report.removed, report.sessions_closed) == (1, 0, 0)
    assert not report.unchanged
    with srv.client("newcomer").Mutex(name) as guard:
        assert not guard.lost


def test_a_removed_user_is_dropped_everywhere_and_its_locks_are_freed(make_server, name):
    srv = make_server(users=BOTH)

    async def main():
        victim, admin = srv.client("billing", pool_size=2), srv.client("reports")
        guard = await victim.Mutex(name).acquire()
        await admin.connect()
        assert await admin.Mutex(name).try_acquire() is None

        srv.write_users({"reports": True})  # billing is gone
        report = await admin.maintenance.reset_users()
        assert (report.removed, report.sessions_closed, report.invoker_dropped) == (1, 1, False), report
        assert report.connections_closed == 2, "both pool connections were closed by the server"

        # The server released its lock at once; the victim finds out quickly and is not welcome back.
        freed = await admin.Mutex(name).try_acquire()
        assert freed is not None
        await asyncio.wait_for(guard.wait_lost(), 5)
        assert guard.lost and victim.connections == 0
        await freed.release()

    asyncio.run(main())


def test_the_next_holder_sees_abandoned_and_the_removed_client_cannot_log_in(make_server, name):
    srv = make_server(users=BOTH)
    victim, admin = srv.client("billing"), srv.client("reports")
    held = victim.Mutex(name).acquire_sync()
    srv.write_users({"reports": True})
    admin.maintenance.reset_users_sync()
    with admin.Mutex(name, wait=5) as guard:
        assert guard.status is ProcessingStatus.ABANDONED
    with pytest.raises(AuthenticationError):
        victim.Mutex(name + "-x").acquire_sync()  # waits for a connection that cannot come back
    assert held.lost


def test_a_client_can_remove_itself_and_still_gets_its_report(make_server, name):
    srv = make_server(users=BOTH)

    async def main():
        me = srv.client("billing", pool_size=3)
        guard = await me.Mutex(name).acquire()
        srv.write_users({"reports": True})
        report = await me.maintenance.reset_users()  # the reply arrives before the connection is dropped
        assert (report.removed, report.sessions_closed, report.invoker_dropped) == (1, 1, True), report
        await asyncio.wait_for(guard.wait_lost(), 5)
        with pytest.raises(AuthenticationError):
            await me.Mutex(name + "-x").acquire()

    asyncio.run(main())


def test_a_user_that_is_added_back_recovers_without_the_application_doing_anything(make_server, name):
    srv = make_server(users=BOTH)

    async def main():
        a, admin = srv.client("billing"), srv.client("reports")
        await a.connect()
        srv.write_users({"reports": True})
        await admin.maintenance.reset_users()
        await asyncio.sleep(0.5)
        assert a.connections == 0

        srv.write_users(BOTH)
        assert (await admin.maintenance.reset_users()).added == 1
        # `a` has been retrying in the background (backoff up to 5 s); it logs in again by itself.
        guard = await asyncio.wait_for(a.Mutex(name).acquire(), 30)
        assert not guard.lost
        await guard.release()

    asyncio.run(main())


def test_an_unloadable_file_raises_a_maintenance_error_and_changes_nothing(make_server, name):
    srv = make_server(users=BOTH)

    async def main():
        a, b = srv.client("billing"), srv.client("reports")
        async with a.Mutex(name) as guard:
            srv.write_raw("{ definitely not json")
            with pytest.raises(MaintenanceError) as info:
                await b.maintenance.reset_users()
            assert "billing" not in str(info.value), "the reply must not leak names"
            assert not guard.lost
            assert await b.Mutex(name).try_acquire() is None
        srv.write_users(BOTH)
        assert (await b.maintenance.reset_users()).password_changed == 0

    asyncio.run(main())


def test_a_changed_password_only_affects_new_logins(make_server, name):
    srv = make_server(users=BOTH)
    live = srv.client("billing")
    guard = live.Mutex(name).acquire_sync()

    # Same user, different password hash: the live session carries on, new logins need the new one.
    import json

    doc = json.loads(srv.users_file.read_text())
    from conftest import _hash

    for user in doc["users"]:
        if user["client_id"] == "billing":
            user["password_hash"] = _hash("rotated")
    srv.write_raw(json.dumps(doc))
    report = srv.client("reports").maintenance.reset_users_sync()
    assert (report.password_changed, report.sessions_closed) == (1, 0)

    assert not guard.lost
    with pytest.raises(AuthenticationError):
        DistLockClient.from_url(srv.url("billing")).connect_sync()  # old password
    DistLockClient.from_url(srv.url("billing", password="rotated")).connect_sync()
    guard.release_sync()
