"""Enumerations of the public API."""

from __future__ import annotations

import enum
from dataclasses import dataclass


class LockMode(str, enum.Enum):
    """How a lock from :meth:`DistLockClient.Lock` behaves.

    ``READ`` and ``WRITE`` are the two sides of one read/write lock: any number of readers may hold
    it together, a writer holds it alone, and a waiting writer is not starved by later readers.
    ``MUTEX`` is a plain exclusive lock that lives in its own namespace: a mutex called ``"x"`` and
    a read/write lock called ``"x"`` never interact.
    """

    READ = "read"
    WRITE = "write"
    MUTEX = "mutex"


class ProcessingStatus(str, enum.Enum):
    """The outcome a holder leaves behind for the next holder of the same lock.

    ``SUCCEEDED``, ``FAILED`` and ``STORED`` are set with :meth:`LockGuard.set_status`.
    ``ABANDONED`` is only ever *read*: the server assigns it when the previous holder's lease
    expired or its session ended while it still held the lock.
    """

    SUCCEEDED = "succeeded"
    FAILED = "failed"
    STORED = "stored"
    ABANDONED = "abandoned"


@dataclass(frozen=True)
class PreviousStatus:
    """What the previous holder left behind, as seen by the next one."""

    status: ProcessingStatus
    #: Fencing token of the holder that set it.
    set_by_token: int
    #: How long ago it was set, in seconds.
    age: float
