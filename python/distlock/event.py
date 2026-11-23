"""Events: a named broadcast, like :class:`asyncio.Event` with a limited memory.

Any number of clients wait on a name; one client sets it, which wakes everyone waiting at that
moment and says how many there were::

    # in many processes
    event = client.Event("deploy-finished")
    version = await event.wait(timeout=60)        # the value the setter sent, or None

    # in one of them
    woken = await client.Event("deploy-finished").set("v1.2.3")
    assert woken == number_of_clients_waiting_right_now

For ``publish_for`` seconds after a set, the event also counts as *published*: a wait that starts
inside that window returns at once with the same value. After the window the event is gone, and
waiters wait for the next set. ``publish_for=0`` wakes only the clients already waiting.
"""

from __future__ import annotations

from typing import Any, Optional

from . import _native, defaults
from ._unset import _UNSET
from .errors import InvalidArgument
from .lock import _acall, _call


class Event:
    """A reusable handle on one event name. Create it with :meth:`DistLockClient.Event`.

    ``wait`` and ``set`` exist in awaitable and blocking (``_sync``) forms, and one ``Event`` can be
    used from many tasks or threads at once.

    **Looping on one event.** An ``Event`` remembers the last publication that its ``wait`` returned
    and only returns newer ones, so::

        event = client.Event("tick")
        while True:
            value = await event.wait()      # never the same publication twice

    does not spin while a publication is still inside its window. Use a fresh ``Event`` (or
    :meth:`reset`) to see the current publication again.

    **Missed wake-ups.** A wait survives dropped connections. If a set happens while this client
    is disconnected, the wake-up cannot be delivered, but the client asks again when it reconnects
    and is answered from the publish window. So an event that must not be missed should be set with
    a window longer than a reconnect takes (the server default is 10 s). With ``publish_for=0`` a
    wake-up lost that way is gone.
    """

    def __init__(
        self,
        native: "_native.NativeClient",
        name: str,
        *,
        namespace: Optional[str],
        timeout: Any = _UNSET,
        publish_for: Any = _UNSET,
    ) -> None:
        if not isinstance(name, str) or not name:
            raise InvalidArgument("an event needs a non-empty name")
        if namespace is not None and ":" in namespace:
            raise InvalidArgument("namespace must not contain ':'")
        _check_seconds("timeout", timeout)
        _check_seconds("publish_for", publish_for)
        self._native = native
        self._seen = 0
        self.name = name
        self.namespace = namespace
        self._default_timeout = timeout
        self._default_publish_for = publish_for

    @property
    def timeout(self) -> Optional[float]:
        """How long :meth:`wait` waits by default: the event's own ``timeout``, else the
        :mod:`~distlock.defaults`. ``None``: as long as the server allows."""
        return defaults.resolve_event_wait(self._default_timeout)

    @property
    def publish_for(self) -> Optional[float]:
        """The default publish window of :meth:`set`: the event's own, else the
        :mod:`~distlock.defaults`. ``None``: the server's default."""
        return defaults.resolve_event_publish_for(self._default_publish_for)

    # ------------------------------------------------------------------ waiting

    def _timeout(self, timeout: Any) -> Optional[float]:
        _check_seconds("timeout", timeout)
        return defaults.resolve_event_wait(self._default_timeout if timeout is _UNSET else timeout)

    def _after(self, after: Optional[int]) -> int:
        if after is None:
            return self._seen
        if not isinstance(after, int) or isinstance(after, bool) or after < 0:
            raise InvalidArgument("after must be a non-negative publication id")
        return after

    def _remember(self, published: "tuple[int, Optional[str], float]") -> Optional[str]:
        publication_id, value, _age = published
        if publication_id > self._seen:
            self._seen = publication_id
        return value

    async def wait(self, timeout: Any = _UNSET, *, after: Optional[int] = None) -> Optional[str]:
        """Wait until the event is set; return the value it was set with (``None`` if none).

        ``timeout`` is in seconds: ``None`` waits as long as the server allows, ``0`` only checks
        whether the event is published right now, anything else raises :class:`LockTimeout` (a
        :class:`TimeoutError`) when it runs out. Defaults to the ``timeout`` the event was made with.

        ``after`` is a publication id (see :attr:`last_id`): only a newer publication ends the wait
        at once. The default is the last one this object returned. Cancelling the task, or pressing
        Ctrl-C in :meth:`wait_sync`, withdraws the wait from the server.
        """
        published = await _acall(
            self._native.wait_event, self.name, self._timeout(timeout), self._after(after), self.namespace
        )
        return self._remember(published)

    def wait_sync(self, timeout: Any = _UNSET, *, after: Optional[int] = None) -> Optional[str]:
        """Blocking :meth:`wait` (releases the GIL while waiting; Ctrl-C works)."""
        published = _call(
            self._native.wait_event_sync, self.name, self._timeout(timeout), self._after(after), self.namespace
        )
        return self._remember(published)

    # ------------------------------------------------------------------ setting

    def _publish_for(self, publish_for: Any) -> Optional[float]:
        _check_seconds("publish_for", publish_for)
        return defaults.resolve_event_publish_for(
            self._default_publish_for if publish_for is _UNSET else publish_for
        )

    @staticmethod
    def _value(value: Any) -> Optional[str]:
        if value is not None and not isinstance(value, str):
            raise InvalidArgument("an event value must be a string or None")
        return value

    async def set(self, value: Optional[str] = None, *, publish_for: Any = _UNSET) -> int:
        """Set the event: wake every client waiting for it, and publish it for ``publish_for`` seconds.

        Returns how many waiting clients were woken. ``value`` is delivered to all of them (and to
        late waiters inside the window); the server caps its size. ``publish_for=None`` (the
        default) uses the server's default window, ``0`` wakes only the clients waiting now, and a
        long window is capped by the server.

        A set is never retried by the client: if the connection fails mid-request the outcome is
        unknown, and :class:`ConnectionFailed` is raised rather than risk waking waiters twice.
        """
        woken, _id, _window = await _acall(
            self._native.set_event, self.name, self._value(value), self._publish_for(publish_for), self.namespace
        )
        return woken

    def set_sync(self, value: Optional[str] = None, *, publish_for: Any = _UNSET) -> int:
        """Blocking :meth:`set`."""
        woken, _id, _window = _call(
            self._native.set_event_sync, self.name, self._value(value), self._publish_for(publish_for), self.namespace
        )
        return woken

    # ------------------------------------------------------------------ memory

    @property
    def last_id(self) -> int:
        """Id of the last publication :meth:`wait` returned (``0`` before the first one)."""
        return self._seen

    def reset(self) -> None:
        """Forget what was already returned, so the next :meth:`wait` may return the publication
        that is currently inside its window again."""
        self._seen = 0

    def __repr__(self) -> str:
        ns = f", namespace={self.namespace!r}" if self.namespace else ""
        return f"Event({self.name!r}{ns}, last_id={self._seen})"


def _check_seconds(name: str, value: Any) -> None:
    if value is None or value is _UNSET:
        return
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not value >= 0:
        raise InvalidArgument(f"{name} must be None or a non-negative number of seconds")
