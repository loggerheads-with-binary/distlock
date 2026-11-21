"""Locks and the guards they hand out.

A :class:`Lock` is a *reusable definition* ("the mutex called nightly-report, 30 s lease"). Every
time you enter it, or call :meth:`Lock.acquire`, the server grants a fresh lease and you get a new
:class:`LockGuard` for it. The same ``Lock`` can be entered again and again, from many tasks or
threads at once.
"""

from __future__ import annotations

import contextvars
import logging
from typing import Any, Awaitable, Callable, Optional, Tuple, TypeVar, Union

from . import _native
from .enums import LockMode, PreviousStatus, ProcessingStatus
from .errors import DistLockError, InvalidArgument, LockLost, translate

_log = logging.getLogger("distlock")

T = TypeVar("T")

#: Marks "argument not given", so ``wait=None`` (wait as long as the server allows) stays meaningful.
_UNSET: Any = object()

#: The guards entered with ``with lock`` / ``async with lock`` in the current context, innermost last.
#: A context variable (not an attribute of the Lock) so one Lock can be used by many tasks at once.
_active: "contextvars.ContextVar[Tuple[Tuple[Lock, LockGuard], ...]]" = contextvars.ContextVar(
    "distlock_active", default=()
)


def _call(fn: Callable[..., T], *args: Any) -> T:
    try:
        return fn(*args)
    except _native.NativeError as e:
        raise translate(e) from None


async def _acall(fn: Callable[..., Awaitable[T]], *args: Any) -> T:
    try:
        return await fn(*args)
    except _native.NativeError as e:
        raise translate(e) from None


class Lock:
    """A reusable lock. Create one with :meth:`DistLockClient.Lock` and friends.

    Use it as a context manager (``async with`` or plain ``with``), or acquire explicitly::

        async with client.Mutex("nightly-report") as guard:
            ...

        guard = await lock.acquire(wait=5)     # raises LockTimeout after 5 s
        guard = await lock.try_acquire()       # None if it is taken right now
    """

    def __init__(
        self,
        native: "_native.NativeClient",
        name: str,
        *,
        kind: str,
        ttl: Optional[float],
        namespace: Optional[str],
        wait: Optional[float],
        capacity: int = 0,
        permits: int = 1,
    ) -> None:
        if not isinstance(name, str) or not name:
            raise InvalidArgument("a lock needs a non-empty name")
        if ttl is not None and not ttl > 0:
            raise InvalidArgument("ttl must be a positive number of seconds")
        if wait is not None and not wait >= 0:
            raise InvalidArgument("wait must be zero or a positive number of seconds")
        if namespace is not None and ":" in namespace:
            raise InvalidArgument("namespace must not contain ':'")
        self._native = native
        self._kind = kind
        self._capacity = capacity
        self._permits = permits
        self.name = name
        self.ttl = ttl
        self.namespace = namespace
        self.wait = wait

    @property
    def mode(self) -> Optional[LockMode]:
        """``READ``, ``WRITE`` or ``MUTEX``; ``None`` for a semaphore."""
        return LockMode(self._kind) if self._kind in ("read", "write", "mutex") else None

    # ------------------------------------------------------------------ acquiring

    def _wait(self, wait: Any) -> Optional[float]:
        return self.wait if wait is _UNSET else wait

    async def acquire(self, *, wait: Any = _UNSET) -> "LockGuard":
        """Wait for the lock and return its guard.

        ``wait`` is in seconds: ``None`` waits as long as the server allows, ``0`` does not wait
        (:class:`~distlock.errors.LockBusy` if taken), anything else raises
        :class:`~distlock.errors.LockTimeout` when it runs out. Defaults to the lock's own ``wait``.
        """
        native_lock = await _acall(
            self._native.acquire,
            self.name,
            self._kind,
            self.ttl,
            self._wait(wait),
            self.namespace,
            self._capacity,
            self._permits,
        )
        return LockGuard(native_lock, self)

    async def try_acquire(self) -> Optional["LockGuard"]:
        """The guard if the lock was free right now, otherwise ``None``. Never waits."""
        native_lock = await _acall(
            self._native.try_acquire, self.name, self._kind, self.ttl, self.namespace, self._capacity, self._permits
        )
        return None if native_lock is None else LockGuard(native_lock, self)

    def acquire_sync(self, *, wait: Any = _UNSET) -> "LockGuard":
        """Blocking :meth:`acquire` (releases the GIL while waiting; Ctrl-C works)."""
        native_lock = _call(
            self._native.acquire_sync,
            self.name,
            self._kind,
            self.ttl,
            self._wait(wait),
            self.namespace,
            self._capacity,
            self._permits,
        )
        return LockGuard(native_lock, self)

    def try_acquire_sync(self) -> Optional["LockGuard"]:
        """Blocking :meth:`try_acquire`."""
        native_lock = _call(
            self._native.try_acquire_sync,
            self.name,
            self._kind,
            self.ttl,
            self.namespace,
            self._capacity,
            self._permits,
        )
        return None if native_lock is None else LockGuard(native_lock, self)

    # ------------------------------------------------------------------ context managers

    def _push(self, guard: "LockGuard") -> None:
        _active.set(_active.get() + ((self, guard),))

    def _pop(self) -> "LockGuard":
        stack = _active.get()
        for i in range(len(stack) - 1, -1, -1):
            if stack[i][0] is self:
                _active.set(stack[:i] + stack[i + 1 :])
                return stack[i][1]
        raise RuntimeError(f"{self!r} was exited without being entered in this context")

    async def __aenter__(self) -> "LockGuard":
        guard = await self.acquire()
        self._push(guard)
        return guard

    async def __aexit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        await self._pop()._finish(raise_errors=exc_type is None)

    def __enter__(self) -> "LockGuard":
        guard = self.acquire_sync()
        self._push(guard)
        return guard

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self._pop()._finish_sync(raise_errors=exc_type is None)

    def __repr__(self) -> str:
        what = self.mode.value if self.mode else f"semaphore({self._capacity})"
        ns = f", namespace={self.namespace!r}" if self.namespace else ""
        return f"Lock({self.name!r}, {what}, ttl={self.ttl}{ns})"


class LockGuard:
    """One grant of a lock. Valid from acquire until release (the end of the ``with`` block).

    The lease is renewed in the background for as long as the guard lives, so a long job does not
    lose its lock. If the lease is lost anyway (the server restarted, a network partition outlasted
    it), :attr:`lost` turns true and leaving the ``with`` block raises
    :class:`~distlock.errors.LockLost`.
    """

    def __init__(self, native: "_native.NativeLock", lock: Lock) -> None:
        self._native = native
        self._lock = lock
        self._status: Optional[ProcessingStatus] = None
        self._status_ttl: Optional[float] = None
        self._released = False

    # ------------------------------------------------------------------ facts about the grant

    @property
    def name(self) -> str:
        """The lock name as you gave it (without namespace)."""
        return self._native.name

    @property
    def mode(self) -> Optional[LockMode]:
        return self._lock.mode

    @property
    def fencing_token(self) -> int:
        """Strictly increasing across all grants on the server. Hand it to the resource you protect
        and have that resource reject writes carrying an older token."""
        return self._native.fencing_token

    @property
    def ttl(self) -> float:
        """The lease length the server granted, in seconds (it may cap what you asked for)."""
        return self._native.ttl

    # ------------------------------------------------------------------ the previous holder's status

    @property
    def previous(self) -> Optional[PreviousStatus]:
        """What the previous holder left behind (with who set it and when), or ``None``."""
        raw = self._native.previous
        if raw is None:
            return None
        status, set_by, age = raw
        return PreviousStatus(ProcessingStatus(status), set_by, age)

    @property
    def status(self) -> Optional[ProcessingStatus]:
        """The previous holder's status: ``SUCCEEDED``, ``FAILED``, ``STORED``, ``ABANDONED``
        (its lease expired, or its session ended, while it held the lock), or ``None``."""
        previous = self.previous
        return None if previous is None else previous.status

    def set_status(self, status: Union[ProcessingStatus, str], *, ttl: Optional[float] = None) -> None:
        """Record this holder's outcome for whoever takes the lock next.

        Applied when the guard is released (the end of the ``with`` block). Only mutex and write
        locks can leave a status. ``ttl`` is how long, in seconds, it stays readable (default: the
        server's).
        """
        status = ProcessingStatus(status)
        if status is ProcessingStatus.ABANDONED:
            raise InvalidArgument("ABANDONED is assigned by the server and cannot be set")
        if self._lock.mode not in (LockMode.MUTEX, LockMode.WRITE):
            raise InvalidArgument("only mutex and write locks can leave a status")
        if ttl is not None and not ttl > 0:
            raise InvalidArgument("ttl must be a positive number of seconds")
        self._status = status
        self._status_ttl = ttl

    # ------------------------------------------------------------------ health

    @property
    def lost(self) -> bool:
        """True once the lease is known to be gone. Stop touching what the lock protects."""
        return bool(self._native.is_lost)

    def check(self) -> None:
        """Raise :class:`~distlock.errors.LockLost` if the lease is gone; call it before the
        irreversible steps of a long job."""
        if self.lost:
            raise LockLost(f"the lease on {self.name!r} was lost")

    async def wait_lost(self) -> None:
        """Returns when the lease is lost (never, for a lock that stays healthy)."""
        await _acall(self._native.wait_lost)

    def wait_lost_sync(self) -> None:
        _call(self._native.wait_lost_sync)

    async def extend(self, ttl: float) -> float:
        """Lengthen the lease to ``ttl`` seconds from now; returns the remaining lease. The
        background renewal already keeps it alive, so you rarely need this."""
        return await _acall(self._native.extend, ttl)

    def extend_sync(self, ttl: float) -> float:
        return _call(self._native.extend_sync, ttl)

    # ------------------------------------------------------------------ releasing

    @property
    def released(self) -> bool:
        return self._released

    def _release_args(self) -> Tuple[Optional[str], Optional[float]]:
        return (None if self._status is None else self._status.value, self._status_ttl)

    async def release(self) -> None:
        """Release now (applying :meth:`set_status`). Idempotent. Raises
        :class:`~distlock.errors.LockLost` if the lease had been lost."""
        await self._finish(raise_errors=True)

    def release_sync(self) -> None:
        self._finish_sync(raise_errors=True)

    async def _finish(self, *, raise_errors: bool) -> None:
        if self._released:
            return
        self._released = True
        was_lost = self.lost
        try:
            await _acall(self._native.release, *self._release_args())
        except DistLockError as e:
            self._on_release_error(e, raise_errors)
            return
        if was_lost and raise_errors:
            raise LockLost(f"the lease on {self.name!r} was lost while it was held")

    def _finish_sync(self, *, raise_errors: bool) -> None:
        if self._released:
            return
        self._released = True
        was_lost = self.lost
        try:
            _call(self._native.release_sync, *self._release_args())
        except DistLockError as e:
            self._on_release_error(e, raise_errors)
            return
        if was_lost and raise_errors:
            raise LockLost(f"the lease on {self.name!r} was lost while it was held")

    def _on_release_error(self, error: DistLockError, raise_errors: bool) -> None:
        if raise_errors:
            raise error
        # The block is already failing with its own exception; do not mask it with ours.
        _log.warning("could not release %r while handling another error: %s", self.name, error)

    # The guard is itself a context manager, for ``guard = await lock.acquire()`` followed by
    # ``async with guard:`` (the lock is released on exit).

    async def __aenter__(self) -> "LockGuard":
        return self

    async def __aexit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        await self._finish(raise_errors=exc_type is None)

    def __enter__(self) -> "LockGuard":
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self._finish_sync(raise_errors=exc_type is None)

    def __repr__(self) -> str:
        state = "released" if self._released else ("lost" if self.lost else "held")
        return f"LockGuard({self.name!r}, token={self.fencing_token}, {state})"
