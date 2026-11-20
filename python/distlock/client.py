"""The client: one session on the server, shared by a pool of connections."""

from __future__ import annotations

import os
from typing import Any, Optional, Union

from pydantic import ValidationError

from . import _native
from .enums import LockMode
from .errors import ConfigError, translate
from .event import Event as _Event
from .lock import Lock, _acall, _call
from .maintenance import Maintenance
from .settings import ClientSettings, validation_message


class DistLockClient:
    """Entry point. Create it, then make locks from it::

        client = DistLockClient.from_url("distlock://billing:s3cret@10.0.0.5:7878/billing?pool_size=5&default_ttl=30")

        async with client.mutex("nightly-report") as guard:
            if guard.status is ProcessingStatus.SUCCEEDED:
                return                      # someone already did it
            ...
            guard.set_status(ProcessingStatus.SUCCEEDED)

    Creating a client does not touch the network: the pool connects the first time a lock is
    acquired (or when you call :meth:`connect`). The session id, assigned by the server when the
    pool is set up, is available as :attr:`session_id`.

    One client is cheap to share between tasks and threads; create one per application, not per lock.
    """

    def __init__(self, settings: Optional[ClientSettings] = None, /, **fields: Any) -> None:
        if settings is not None and fields:
            raise TypeError("pass either a ClientSettings or keyword fields, not both")
        if settings is None:
            try:
                settings = ClientSettings(**fields)
            except ValidationError as e:
                raise ConfigError(validation_message(e)) from None
        self._settings = settings
        try:
            self._native = _native.NativeClient(
                settings.client_id,
                settings.password.get_secret_value(),
                settings.host,
                settings.port,
                settings.namespace,
                settings.pool_size,
                settings.tls,
                settings.tls_verify,
                settings.tls_ca_file,
                settings.default_ttl,
            )
        except _native.NativeError as e:
            raise translate(e) from None
        self._maintenance = Maintenance(self._native)

    # ------------------------------------------------------------------ construction

    @classmethod
    def from_url(cls, url: str) -> "DistLockClient":
        """``distlock://client_id:password@host[:port][/namespace][?pool_size=&tls=&tls_verify=&tls_ca_file=&default_ttl=]``

        See :meth:`ClientSettings.from_url` for the details.
        """
        return cls(ClientSettings.from_url(url))

    @classmethod
    def from_env(cls, variable: str = "DISTLOCK_URL") -> "DistLockClient":
        """Like :meth:`from_url`, reading the URL from an environment variable."""
        url = os.environ.get(variable)
        if not url:
            raise ConfigError(f"environment variable {variable} is not set")
        return cls.from_url(url)

    # ------------------------------------------------------------------ introspection

    @property
    def settings(self) -> ClientSettings:
        return self._settings

    @property
    def session_id(self) -> Optional[int]:
        """The server-assigned session id, or ``None`` until the pool has connected.

        All the pool's connections share this one session. It changes only if the server forgot the
        session (a restart) and the client had to log in again.
        """
        return self._native.session_id

    @property
    def maintenance(self) -> Maintenance:
        """Administrative commands: ``await client.maintenance.reset_users()``."""
        return self._maintenance

    @property
    def connections(self) -> int:
        """How many of the pool's TCP connections are up right now (0 before the first use)."""
        return self._native.connections

    @property
    def reconnects(self) -> int:
        """How many times a pool connection has been re-established after dropping."""
        return self._native.reconnects

    async def ping(self) -> float:
        """Round-trip time to the server, in seconds. A cheap health check; connects if needed."""
        return await _acall(self._native.ping)

    def ping_sync(self) -> float:
        return _call(self._native.ping_sync)

    # ------------------------------------------------------------------ making locks

    def lock(
        self,
        name: str,
        ttl: Optional[float] = None,
        namespace: Optional[str] = None,
        mode: Union[LockMode, str] = LockMode.MUTEX,
        *,
        wait: Optional[float] = None,
    ) -> Lock:
        """A reusable lock called ``name``.

        :param ttl: lease length in seconds (renewed automatically while held); default: the
            client's ``default_ttl``.
        :param namespace: overrides the client's namespace for this lock; ``""`` means none.
        :param mode: ``LockMode.MUTEX`` (default), ``READ`` or ``WRITE``.
        :param wait: how long, in seconds, acquiring may wait: ``None`` (default) as long as the
            server allows, ``0`` not at all.

        Nothing is acquired until you enter the lock or call :meth:`Lock.acquire`.
        """
        mode = LockMode(mode)
        return Lock(self._native, name, kind=mode.value, ttl=ttl, namespace=namespace, wait=wait)

    def rlock(
        self, name: str, ttl: Optional[float] = None, namespace: Optional[str] = None, *, wait: Optional[float] = None
    ) -> Lock:
        """The shared (reader) side of the read/write lock ``name``: many readers at once, but never
        alongside a writer."""
        return self.lock(name, ttl, namespace, LockMode.READ, wait=wait)

    def wlock(
        self, name: str, ttl: Optional[float] = None, namespace: Optional[str] = None, *, wait: Optional[float] = None
    ) -> Lock:
        """The exclusive (writer) side of the read/write lock ``name``: blocks, and is blocked by,
        every reader and every other writer of that name."""
        return self.lock(name, ttl, namespace, LockMode.WRITE, wait=wait)

    def mutex(
        self, name: str, ttl: Optional[float] = None, namespace: Optional[str] = None, *, wait: Optional[float] = None
    ) -> Lock:
        """A plain exclusive lock. Mutexes are a separate space from read/write locks: a mutex and
        an rlock/wlock with the same name do not conflict."""
        return self.lock(name, ttl, namespace, LockMode.MUTEX, wait=wait)

    def semaphore(
        self,
        name: str,
        capacity: int,
        permits: int = 1,
        ttl: Optional[float] = None,
        namespace: Optional[str] = None,
        *,
        wait: Optional[float] = None,
    ) -> Lock:
        """At most ``capacity`` holders at a time (each holding ``permits``).

        The first acquirer defines the capacity; later ones must use the same. The semaphore
        disappears once its last holder and waiter are gone, so a redeployed application can start
        over with a different capacity.
        """
        if not isinstance(capacity, int) or capacity < 1:
            raise ConfigError("capacity must be a positive integer")
        if not isinstance(permits, int) or not 1 <= permits <= capacity:
            raise ConfigError("permits must be between 1 and the capacity")
        return Lock(
            self._native,
            name,
            kind="semaphore",
            ttl=ttl,
            namespace=namespace,
            wait=wait,
            capacity=capacity,
            permits=permits,
        )

    # ------------------------------------------------------------------ lifecycle

    def event(
        self,
        name: str,
        *,
        namespace: Optional[str] = None,
        timeout: Optional[float] = None,
        publish_for: Optional[float] = None,
    ) -> _Event:
        """A named broadcast, like :class:`asyncio.Event` with a limited memory::

            val = await client.event("deploy-finished").wait()
            woken = await client.event("deploy-finished").set("v1.2.3")

        :param namespace: overrides the client's namespace; ``""`` means none.
        :param timeout: how long :meth:`Event.wait` waits by default: ``None`` as long as the server
            allows, ``0`` only checks.
        :param publish_for: default publish window of :meth:`Event.set`, in seconds: how long after a
            set a late waiter is still answered at once. ``None``: the server's default.

        Events live apart from locks: an event and a lock with the same name do not interact.
        Nothing happens on the server until you wait or set. See :class:`Event`.
        """
        return _Event(self._native, name, namespace=namespace, timeout=timeout, publish_for=publish_for)

    #: The same as :meth:`event`, for code that reads better with a capital letter.
    Event = event

    async def connect(self) -> None:
        """Connect now instead of on first use (to fail fast on bad credentials)."""
        await _acall(self._native.connect)

    def connect_sync(self) -> None:
        _call(self._native.connect_sync)

    async def close(self) -> None:
        """End the session: the server releases every lock this client holds, and the pool stops."""
        await _acall(self._native.close)

    def close_sync(self) -> None:
        _call(self._native.close_sync)

    async def __aenter__(self) -> "DistLockClient":
        await self.connect()
        return self

    async def __aexit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        await self.close()

    def __enter__(self) -> "DistLockClient":
        self.connect_sync()
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.close_sync()

    def __repr__(self) -> str:
        s = self._settings
        ns = f", namespace={s.namespace!r}" if s.namespace else ""
        return (
            f"DistLockClient(client_id={s.client_id!r}, host={s.host!r}, port={s.port}{ns}, "
            f"pool_size={s.pool_size}, session_id={self.session_id})"
        )
