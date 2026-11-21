"""``client.stats()``: the server's health figures."""
from __future__ import annotations

import asyncio

from distlock import ServerStats

BOTH = {"billing": True, "reports": True}


def test_stats_count_clients_and_locks(make_server, name):
    srv = make_server(users=BOTH)

    async def main():
        a, b = srv.client("billing"), srv.client("reports")
        async with a.Mutex(name):
            s = await b.stats()
            assert isinstance(s, ServerStats)
            assert s.connected_clients == 2 and s.locks_held == 1 and s.requests_queued == 0
            assert s.system is not None and 0 <= s.system.cpu_percent <= 100
            assert s.process is not None and 0 <= s.process.cpu_percent <= 100
            assert s.process.memory_bytes and s.process.open_files

    asyncio.run(main())


def test_stats_sync(make_server):
    srv = make_server(users=BOTH)
    s = srv.client("billing").stats_sync()
    assert s.connected_clients == 1 and not s.in_startup_grace
