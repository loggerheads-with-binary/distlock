"""Exceptions raised by distlock.

Everything derives from :class:`DistLockError`, so ``except DistLockError`` catches them all.
Where a standard exception is a natural fit, the distlock one also derives from it
(``LockTimeout`` is a ``TimeoutError``, ``ConnectionFailed`` a ``ConnectionError``, and so on).
"""

from __future__ import annotations

from typing import Dict, Type


class DistLockError(Exception):
    """Base class of every error raised by distlock."""


class InvalidArgument(DistLockError, ValueError):
    """A call was made with an argument the client or the server rejects."""


class ConfigError(InvalidArgument):
    """The client settings, or the connection URL, are invalid."""


class AuthenticationError(DistLockError):
    """The server rejected the client id / password."""


class ConnectionFailed(DistLockError, ConnectionError):
    """The server could not be reached, or is not ready to accept lock requests.

    The client keeps reconnecting in the background; retrying later may succeed.
    """


class SessionLost(DistLockError):
    """The server forgot this client's session (it restarted, or the session expired).

    Every lock held through the old session is gone. The client logs in again by itself, so new
    acquires work, but anything protected by the old locks must be treated as unprotected.
    """


class LockTimeout(DistLockError, TimeoutError):
    """The lock was not obtained, or the event was not set, within the wait limit."""


class LockBusy(DistLockError):
    """A zero-wait acquire found the lock taken. (:meth:`Lock.try_acquire` returns ``None`` instead.)"""


class LockLost(DistLockError):
    """The lease on a held lock expired (or the session was lost) before it was released.

    Whatever the lock protected may have been touched by someone else meanwhile.
    """


class TlsError(ConnectionFailed):
    """TLS could not be set up (an unreadable CA file, an invalid host name), or the server's
    certificate was rejected under the verification settings in use."""
class ClientClosed(DistLockError):
    """The client has been closed."""


class MaintenanceError(DistLockError):
    """A maintenance command could not be carried out, and nothing was changed (for example the
    server's users file could not be loaded). The server's log has the details."""


class ServerError(DistLockError):
    """The server reported an error the client does not map to anything more specific."""


_BY_KIND: Dict[str, Type[DistLockError]] = {
    "config": ConfigError,
    "tls": TlsError,
    "invalid_argument": InvalidArgument,
    "auth": AuthenticationError,
    "connect": ConnectionFailed,
    "not_connected": ConnectionFailed,
    "connection_lost": ConnectionFailed,
    "unavailable": ConnectionFailed,
    "session_lost": SessionLost,
    "timeout": LockTimeout,
    "busy": LockBusy,
    "lease_lost": LockLost,
    "closed": ClientClosed,
    "maintenance": MaintenanceError,
}


def translate(native_error: BaseException) -> DistLockError:
    """Turn the native layer's ``NativeError(kind, message)`` into the public exception."""
    args = native_error.args
    kind = args[0] if len(args) > 0 else "unknown"
    message = args[1] if len(args) > 1 else str(native_error)
    return _BY_KIND.get(kind, ServerError)(message)
