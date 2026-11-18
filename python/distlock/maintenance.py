"""Administrative commands: ``client.maintenance``."""

from __future__ import annotations

from dataclasses import dataclass

from . import _native
from .lock import _acall, _call


@dataclass(frozen=True)
class ResetUsersReport:
    """What :meth:`Maintenance.reset_users` did. Counts only: the names are in the server's log, so
    this cannot be used to enumerate other applications."""

    added: int
    removed: int
    disabled: int
    enabled: int
    password_changed: int
    #: Sessions of removed or disabled users that were ended: their locks released (after the server's
    #: lock-delay) and their waiting requests cancelled.
    sessions_closed: int
    #: Their TCP connections, closed by the server.
    connections_closed: int
    #: The caller itself was among the users thrown out. The report still reached it; its connection
    #: closed right after, and so did the rest of its pool.
    invoker_dropped: bool

    @property
    def unchanged(self) -> bool:
        """Nothing changed at all (the file was the same as the one already in force)."""
        return not (
            self.added
            or self.removed
            or self.disabled
            or self.enabled
            or self.password_changed
            or self.sessions_closed
            or self.connections_closed
            or self.invoker_dropped
        )


class Maintenance:
    """Administrative commands, reached as ``client.maintenance``.

    There is no per-user authorization yet, so any logged-in client may use them; each command is
    therefore safe and idempotent.
    """

    def __init__(self, native: "_native.NativeClient") -> None:
        self._native = native

    async def reset_users(self) -> ResetUsersReport:
        """Tell the server to re-read its users file and swap it in.

        Logins are never refused or delayed meanwhile. Users that are gone from the file, or now
        disabled, are thrown out: their sessions end and **every one of their TCP connections is
        closed by the server, including this client's own if it was thrown out** (you still get the
        report, with ``invoker_dropped`` set; afterwards this client cannot log in and its calls raise
        :class:`~distlock.errors.AuthenticationError`, until the user is added back, which it then
        notices by itself). Live sessions of users whose password merely changed are left alone.

        Raises :class:`~distlock.errors.MaintenanceError`, changing nothing, if the file cannot be
        loaded. An unchanged file is a no-op, so calling this again is always safe.
        """
        return ResetUsersReport(*await _acall(self._native.reset_users))

    def reset_users_sync(self) -> ResetUsersReport:
        """Blocking :meth:`reset_users`."""
        return ResetUsersReport(*_call(self._native.reset_users_sync))
