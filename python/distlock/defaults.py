"""Process-wide defaults for lease lengths, waits and event windows.

Every lock, event or primitive that does not say what it wants falls back to these, *when it is
used* (not when it was created), so a change applies to objects made earlier too::

    from distlock.defaults import set_default_wait, set_default_semaphore_wait

    set_default_wait(10)              # every lock and event waits at most 10 s
    set_default_semaphore_wait(60)    # ...except semaphores, which wait up to a minute

Cascade
-------
For one use, the first value that is set wins:

========================  =====================================================================
lock ``wait``             its own ``wait``; ``DEFAULT_<KIND>_WAIT``; ``DEFAULT_WAIT``; as long as the server allows
event ``timeout``         its own ``timeout``; ``DEFAULT_EVENT_WAIT``; ``DEFAULT_WAIT``; as long as the server allows
lock ``ttl``              its own ``ttl``; ``DEFAULT_<KIND>_TTL``; ``DEFAULT_TTL``; the client's ``default_ttl``
event ``publish_for``     its own ``publish_for``; ``DEFAULT_EVENT_PUBLISH_FOR``; the server's default
========================  =====================================================================

``<KIND>`` is ``MUTEX``, ``READ``, ``WRITE`` or ``SEMAPHORE``. With nothing set, behaviour is exactly
what it was before these defaults existed.

A wait default of ``None`` means "as long as the server allows". It is not the same as never having
set one: a kind set to ``None`` stops the cascade there, while a kind that was reset lets it go on.
Reset a default with its ``reset_default_*`` function (or :func:`reset_all`).

Python and Rust defaults are separate: the Rust crate has its own ``defaults`` module, and values set
here are never seen by it, or the other way round.
"""

from __future__ import annotations

import math
from typing import Any, Optional

from ._unset import _UNSET
from .errors import InvalidArgument

#: The server counts leases in 32-bit milliseconds.
_MAX_TTL_SECONDS = 4_000_000.0

# ---------------------------------------------------------------------------------------------- values
# ``_UNSET``: not set (the next level of the cascade applies). For waits, ``None``: as long as the
# server allows. Read them with the ``resolve_*`` functions rather than directly.

DEFAULT_TTL: Any = _UNSET
DEFAULT_MUTEX_TTL: Any = _UNSET
DEFAULT_READ_TTL: Any = _UNSET
DEFAULT_WRITE_TTL: Any = _UNSET
DEFAULT_SEMAPHORE_TTL: Any = _UNSET

DEFAULT_WAIT: Any = _UNSET
DEFAULT_MUTEX_WAIT: Any = _UNSET
DEFAULT_READ_WAIT: Any = _UNSET
DEFAULT_WRITE_WAIT: Any = _UNSET
DEFAULT_SEMAPHORE_WAIT: Any = _UNSET
DEFAULT_EVENT_WAIT: Any = _UNSET

DEFAULT_EVENT_PUBLISH_FOR: Any = _UNSET

_KINDS = ("mutex", "read", "write", "semaphore")


# ---------------------------------------------------------------------------------------------- checks


def _ttl(name: str, value: float) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not 0 < value <= _MAX_TTL_SECONDS:
        raise InvalidArgument(f"{name} must be a positive number of seconds (at most {_MAX_TTL_SECONDS:g})")
    return value


def _wait(name: str, value: Optional[float]) -> Optional[float]:
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)) or math.isnan(value) or value < 0:
        raise InvalidArgument(f"{name} must be None or a non-negative number of seconds")
    return value


def _window(name: str, value: float) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or math.isnan(value) or value < 0:
        raise InvalidArgument(f"{name} must be a non-negative number of seconds")
    return value


# ---------------------------------------------------------------------------------------------- lease length


def set_default_ttl(seconds: float) -> None:
    """Lease length, in seconds, of every kind of lock that sets no ``ttl`` (and has no kind default)."""
    global DEFAULT_TTL
    DEFAULT_TTL = _ttl("the default ttl", seconds)


def set_default_mutex_ttl(seconds: float) -> None:
    """Lease length of mutexes that set no ``ttl``."""
    global DEFAULT_MUTEX_TTL
    DEFAULT_MUTEX_TTL = _ttl("the default mutex ttl", seconds)


def set_default_read_ttl(seconds: float) -> None:
    """Lease length of read locks that set no ``ttl``."""
    global DEFAULT_READ_TTL
    DEFAULT_READ_TTL = _ttl("the default read ttl", seconds)


def set_default_write_ttl(seconds: float) -> None:
    """Lease length of write locks that set no ``ttl``."""
    global DEFAULT_WRITE_TTL
    DEFAULT_WRITE_TTL = _ttl("the default write ttl", seconds)


def set_default_semaphore_ttl(seconds: float) -> None:
    """Lease length of semaphores that set no ``ttl``."""
    global DEFAULT_SEMAPHORE_TTL
    DEFAULT_SEMAPHORE_TTL = _ttl("the default semaphore ttl", seconds)


def reset_default_ttl() -> None:
    global DEFAULT_TTL
    DEFAULT_TTL = _UNSET


def reset_default_mutex_ttl() -> None:
    global DEFAULT_MUTEX_TTL
    DEFAULT_MUTEX_TTL = _UNSET


def reset_default_read_ttl() -> None:
    global DEFAULT_READ_TTL
    DEFAULT_READ_TTL = _UNSET


def reset_default_write_ttl() -> None:
    global DEFAULT_WRITE_TTL
    DEFAULT_WRITE_TTL = _UNSET


def reset_default_semaphore_ttl() -> None:
    global DEFAULT_SEMAPHORE_TTL
    DEFAULT_SEMAPHORE_TTL = _UNSET


# ---------------------------------------------------------------------------------------------- waiting


def set_default_wait(seconds: Optional[float]) -> None:
    """How long every lock and event waits when it sets no ``wait`` / ``timeout`` (and has no kind
    default). ``None``: as long as the server allows; ``0``: not at all."""
    global DEFAULT_WAIT
    DEFAULT_WAIT = _wait("the default wait", seconds)


def set_default_mutex_wait(seconds: Optional[float]) -> None:
    """How long mutexes wait when they set no ``wait``."""
    global DEFAULT_MUTEX_WAIT
    DEFAULT_MUTEX_WAIT = _wait("the default mutex wait", seconds)


def set_default_read_wait(seconds: Optional[float]) -> None:
    """How long read locks wait when they set no ``wait``."""
    global DEFAULT_READ_WAIT
    DEFAULT_READ_WAIT = _wait("the default read wait", seconds)


def set_default_write_wait(seconds: Optional[float]) -> None:
    """How long write locks wait when they set no ``wait``."""
    global DEFAULT_WRITE_WAIT
    DEFAULT_WRITE_WAIT = _wait("the default write wait", seconds)


def set_default_semaphore_wait(seconds: Optional[float]) -> None:
    """How long semaphores wait when they set no ``wait``."""
    global DEFAULT_SEMAPHORE_WAIT
    DEFAULT_SEMAPHORE_WAIT = _wait("the default semaphore wait", seconds)


def set_default_event_wait(seconds: Optional[float]) -> None:
    """How long :meth:`Event.wait` waits when no ``timeout`` is given."""
    global DEFAULT_EVENT_WAIT
    DEFAULT_EVENT_WAIT = _wait("the default event wait", seconds)


def reset_default_wait() -> None:
    global DEFAULT_WAIT
    DEFAULT_WAIT = _UNSET


def reset_default_mutex_wait() -> None:
    global DEFAULT_MUTEX_WAIT
    DEFAULT_MUTEX_WAIT = _UNSET


def reset_default_read_wait() -> None:
    global DEFAULT_READ_WAIT
    DEFAULT_READ_WAIT = _UNSET


def reset_default_write_wait() -> None:
    global DEFAULT_WRITE_WAIT
    DEFAULT_WRITE_WAIT = _UNSET


def reset_default_semaphore_wait() -> None:
    global DEFAULT_SEMAPHORE_WAIT
    DEFAULT_SEMAPHORE_WAIT = _UNSET


def reset_default_event_wait() -> None:
    global DEFAULT_EVENT_WAIT
    DEFAULT_EVENT_WAIT = _UNSET


# ---------------------------------------------------------------------------------------------- events


def set_default_event_publish_for(seconds: float) -> None:
    """The publish window of :meth:`Event.set` calls that give none: how long after a set a late
    waiter is still answered at once."""
    global DEFAULT_EVENT_PUBLISH_FOR
    DEFAULT_EVENT_PUBLISH_FOR = _window("the default publish window", seconds)


def reset_default_event_publish_for() -> None:
    global DEFAULT_EVENT_PUBLISH_FOR
    DEFAULT_EVENT_PUBLISH_FOR = _UNSET


def reset_all() -> None:
    """Forget every default."""
    reset_default_ttl()
    reset_default_mutex_ttl()
    reset_default_read_ttl()
    reset_default_write_ttl()
    reset_default_semaphore_ttl()
    reset_default_wait()
    reset_default_mutex_wait()
    reset_default_read_wait()
    reset_default_write_wait()
    reset_default_semaphore_wait()
    reset_default_event_wait()
    reset_default_event_publish_for()


# ---------------------------------------------------------------------------------------------- resolving
# Used by Lock and Event at the moment they act. ``explicit`` is what the caller gave, or ``_UNSET``.


def _first(*values: Any) -> Any:
    for value in values:
        if value is not _UNSET:
            return value
    return _UNSET


def resolve_ttl(kind: str, explicit: Any = _UNSET) -> Optional[float]:
    """The lease length to ask for, or ``None`` for the client's own ``default_ttl``."""
    if kind not in _KINDS:
        raise InvalidArgument(f"unknown lock kind {kind!r}")
    # ``None`` is "not given" for a ttl: a lease cannot be unlimited.
    given = _UNSET if explicit is None else explicit
    value = _first(given, globals()[f"DEFAULT_{kind.upper()}_TTL"], DEFAULT_TTL)
    return None if value is _UNSET else value


def resolve_wait(kind: str, explicit: Any = _UNSET) -> Optional[float]:
    """The wait for a lock of ``kind``; ``None``: as long as the server allows."""
    if kind not in _KINDS:
        raise InvalidArgument(f"unknown lock kind {kind!r}")
    value = _first(explicit, globals()[f"DEFAULT_{kind.upper()}_WAIT"], DEFAULT_WAIT)
    return None if value is _UNSET else value


def resolve_event_wait(explicit: Any = _UNSET) -> Optional[float]:
    """The wait of an event; ``None``: as long as the server allows."""
    value = _first(explicit, DEFAULT_EVENT_WAIT, DEFAULT_WAIT)
    return None if value is _UNSET else value


def resolve_event_publish_for(explicit: Any = _UNSET) -> Optional[float]:
    """The publish window of an event set; ``None``: the server's default."""
    value = _first(explicit, DEFAULT_EVENT_PUBLISH_FOR)
    return None if value is _UNSET else value
