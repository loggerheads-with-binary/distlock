"""Server health figures: ``client.stats()``."""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional, Tuple


@dataclass(frozen=True)
class SystemStats:
    """The machine the server runs on."""

    total_memory_bytes: int
    used_memory_bytes: int
    cpu_count: int
    #: Whole machine, 0 to 100, measured over a ~200 ms window ending at the snapshot.
    cpu_percent: float
    #: ``None`` where the platform has no load average (Windows).
    load_avg_1m: Optional[float]
    load_avg_5m: Optional[float]
    load_avg_15m: Optional[float]


@dataclass(frozen=True)
class ProcessStats:
    """The server process. ``None`` marks a figure the server's platform cannot report."""

    pid: int
    #: Over the same window as :attr:`SystemStats.cpu_percent`, averaged across all cores (0 to 100).
    cpu_percent: float
    memory_bytes: Optional[int]
    virtual_memory_bytes: Optional[int]
    #: User plus system CPU time since the process started, in seconds.
    cpu_time: Optional[float]
    #: File descriptors (handles), sockets included.
    open_files: Optional[int]
    open_files_limit: Optional[int]


@dataclass(frozen=True)
class ServerStats:
    """What :meth:`DistLockClient.stats` returns: a health check plus the server's own counters."""

    version: str
    #: Seconds since the server's lock engine started.
    uptime: float
    #: Open TCP connections that finished the handshake.
    connections: int
    #: Sessions that exist. A disconnected one lives on until it idles out.
    sessions: int
    #: Sessions with at least one open connection.
    connected_clients: int
    #: Granted leases (a semaphore or read lock counts once per holder).
    locks_held: int
    #: Acquires waiting for a lock.
    requests_queued: int
    #: Names in the lock table, including idle ones kept for their state tag.
    lock_names: int
    #: Event names with waiters or a live publication.
    events: int
    #: ``True`` right after a restart: the server refuses lock requests until the period ends.
    in_startup_grace: bool
    #: Seconds of startup grace left (0 when over).
    startup_grace_remaining: float
    system: Optional[SystemStats]
    process: Optional[ProcessStats]

    @classmethod
    def _from_native(cls, t: Tuple) -> "ServerStats":
        counters, system, process = t
        version, uptime_ms, connections, sessions, clients, held, queued, names, events, grace, grace_ms = counters
        proc = None
        if process is not None:
            pid, cpu, mem, vmem, cpu_ms, files, limit = process
            proc = ProcessStats(pid, cpu, mem, vmem, None if cpu_ms is None else cpu_ms / 1000, files, limit)
        return cls(
            version,
            uptime_ms / 1000,
            connections,
            sessions,
            clients,
            held,
            queued,
            names,
            events,
            grace,
            grace_ms / 1000,
            None if system is None else SystemStats(*system),
            proc,
        )
