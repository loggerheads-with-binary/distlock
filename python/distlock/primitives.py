"""Primitives: what you want from the server, written down without a client.

A primitive is a reusable description ("the read lock called ``catalog``, 20 s lease"). It holds no
connection, so it can be made once and bound as often as you like, to one client or to several::

    readers = ReadLockPrimitive("catalog", ttl=20)

    async with readers.bind(client) as guard:       # the primitive binds itself to a client...
        ...
    async with client.attach(readers) as guard:     # ...or the client attaches the primitive
        ...
    async with readers.bind(other_client) as guard: # the same primitive, on another client
        ...

``bind`` returns the usual :class:`~distlock.Lock` (or :class:`~distlock.Event`), which can be
entered, or acquired explicitly with ``await lock.acquire()``. Nothing happens on the server until then.

Whatever a primitive leaves out (``ttl``, ``wait``, an event's ``timeout`` and ``publish_for``) follows
:mod:`distlock.defaults` when the bound object is used, not when the primitive was made.
"""

from __future__ import annotations

from typing import Any, Optional

from ._unset import _UNSET
from .enums import LockMode
from .errors import ConfigError, InvalidArgument
from .event import Event
from .lock import Lock


def _native_of(client: Any) -> Any:
    native = getattr(client, "_native", None)
    if native is None:
        raise TypeError(f"expected a DistLockClient, got {type(client).__name__}")
    return native


class Primitive:
    """Base class of everything that can be bound to a client."""

    def bind(self, client: Any) -> Any:
        """Bind to ``client``. The primitive is not consumed: bind it again whenever you like."""
        raise NotImplementedError


class _LockPrimitive(Primitive):
    _kind: str = ""

    def __init__(
        self,
        name: str,
        ttl: Optional[float] = None,
        namespace: Optional[str] = None,
        *,
        wait: Any = _UNSET,
    ) -> None:
        self.name = name
        self.ttl = ttl
        self.namespace = namespace
        self.wait = wait
        # Fail now, not at the first bind.
        self._lock(None)

    def _extra(self) -> dict:
        return {}

    def _lock(self, native: Any) -> Lock:
        return Lock(
            native,
            self.name,
            kind=self._kind,
            ttl=self.ttl,
            namespace=self.namespace,
            wait=self.wait,
            **self._extra(),
        )

    @property
    def mode(self) -> Optional[LockMode]:
        return LockMode(self._kind) if self._kind in ("read", "write", "mutex") else None

    def bind(self, client: Any) -> Lock:
        """A :class:`~distlock.Lock` on ``client``. Enter it, or call :meth:`Lock.acquire`."""
        return self._lock(_native_of(client))

    def __repr__(self) -> str:
        ns = f", namespace={self.namespace!r}" if self.namespace else ""
        return f"{type(self).__name__}({self.name!r}, ttl={self.ttl}{ns})"


class MutexPrimitive(_LockPrimitive):
    """An exclusive lock. A separate space from read/write locks of the same name."""

    _kind = "mutex"


class ReadLockPrimitive(_LockPrimitive):
    """The shared side of a read/write lock: many readers at once, never alongside a writer."""

    _kind = "read"


class WriteLockPrimitive(_LockPrimitive):
    """The exclusive side of a read/write lock: blocks, and is blocked by, readers and writers."""

    _kind = "write"


class SemaphorePrimitive(_LockPrimitive):
    """At most ``capacity`` holders at a time, each holding ``permits``. The first acquirer defines
    the capacity."""

    _kind = "semaphore"

    def __init__(
        self,
        name: str,
        capacity: int,
        permits: int = 1,
        ttl: Optional[float] = None,
        namespace: Optional[str] = None,
        *,
        wait: Any = _UNSET,
    ) -> None:
        if not isinstance(capacity, int) or isinstance(capacity, bool) or capacity < 1:
            raise ConfigError("capacity must be a positive integer")
        if not isinstance(permits, int) or isinstance(permits, bool) or not 1 <= permits <= capacity:
            raise ConfigError("permits must be between 1 and the capacity")
        self.capacity = capacity
        self.permits = permits
        super().__init__(name, ttl, namespace, wait=wait)

    def _extra(self) -> dict:
        return {"capacity": self.capacity, "permits": self.permits}

    def __repr__(self) -> str:
        return f"SemaphorePrimitive({self.name!r}, capacity={self.capacity}, permits={self.permits})"


class EventPrimitive(Primitive):
    """A named broadcast; binding gives an :class:`~distlock.Event`."""

    def __init__(
        self,
        name: str,
        *,
        namespace: Optional[str] = None,
        timeout: Any = _UNSET,
        publish_for: Any = _UNSET,
    ) -> None:
        self.name = name
        self.namespace = namespace
        self.timeout = timeout
        self.publish_for = publish_for
        self._event(None)  # validate now

    def _event(self, native: Any) -> Event:
        return Event(
            native,
            self.name,
            namespace=self.namespace,
            timeout=self.timeout,
            publish_for=self.publish_for,
        )

    def bind(self, client: Any) -> Event:
        """A fresh :class:`~distlock.Event` on ``client`` (each one remembers the publications it has seen)."""
        return self._event(_native_of(client))

    def __repr__(self) -> str:
        ns = f", namespace={self.namespace!r}" if self.namespace else ""
        return f"EventPrimitive({self.name!r}{ns})"


def check_primitive(value: Any) -> Primitive:
    if not isinstance(value, Primitive):
        raise InvalidArgument(f"expected a primitive (MutexPrimitive, ReadLockPrimitive, ...), got {type(value).__name__}")
    return value
