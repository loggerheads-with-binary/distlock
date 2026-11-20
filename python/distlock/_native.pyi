"""Type stubs for the compiled module ``distlock._native`` (the PyO3 layer over the Rust client).

Internal: use the public API in ``distlock``. Durations are float seconds; modes and statuses are
lower-case strings. Failures are raised as ``NativeError(kind, message)``.
"""

from typing import Awaitable, Optional, Tuple

__version__: str

class NativeError(Exception):
    """``args == (kind, message)``; ``distlock.errors.translate`` maps it to a public exception."""

class NativeLock:
    @property
    def name(self) -> str: ...
    @property
    def kind(self) -> str: ...
    @property
    def fencing_token(self) -> int: ...
    @property
    def ttl(self) -> float: ...
    @property
    def previous(self) -> Optional[Tuple[str, int, float]]: ...
    @property
    def is_lost(self) -> bool: ...
    def wait_lost(self) -> Awaitable[None]: ...
    def wait_lost_sync(self) -> None: ...
    def release(self, state: Optional[str] = None, state_ttl: Optional[float] = None) -> Awaitable[None]: ...
    def release_sync(self, state: Optional[str] = None, state_ttl: Optional[float] = None) -> None: ...
    def extend(self, ttl: float) -> Awaitable[float]: ...
    def extend_sync(self, ttl: float) -> float: ...

class NativeClient:
    def __init__(
        self,
        client_id: str,
        password: str,
        host: str,
        port: int,
        namespace: Optional[str],
        pool_size: int,
        tls: bool,
        default_ttl: float,
    ) -> None: ...
    @property
    def session_id(self) -> Optional[int]: ...
    @property
    def connections(self) -> int: ...
    @property
    def reconnects(self) -> int: ...
    def ping(self) -> Awaitable[float]: ...
    def ping_sync(self) -> float: ...
    def reset_users(self) -> Awaitable[Tuple[int, int, int, int, int, int, int, bool]]: ...
    def reset_users_sync(self) -> Tuple[int, int, int, int, int, int, int, bool]: ...
    def connect(self) -> Awaitable[None]: ...
    def connect_sync(self) -> None: ...
    def wait_event(
        self, name: str, timeout: Optional[float], after: int, namespace: Optional[str]
    ) -> Awaitable[Tuple[int, Optional[str], float]]: ...
    def wait_event_sync(
        self, name: str, timeout: Optional[float], after: int, namespace: Optional[str]
    ) -> Tuple[int, Optional[str], float]: ...
    def set_event(
        self, name: str, value: Optional[str], publish_for: Optional[float], namespace: Optional[str]
    ) -> Awaitable[Tuple[int, int, float]]: ...
    def set_event_sync(
        self, name: str, value: Optional[str], publish_for: Optional[float], namespace: Optional[str]
    ) -> Tuple[int, int, float]: ...
    def acquire(
        self,
        name: str,
        mode: str,
        ttl: Optional[float],
        wait: Optional[float],
        namespace: Optional[str],
        capacity: int = 0,
        permits: int = 1,
    ) -> Awaitable[NativeLock]: ...
    def acquire_sync(
        self,
        name: str,
        mode: str,
        ttl: Optional[float],
        wait: Optional[float],
        namespace: Optional[str],
        capacity: int = 0,
        permits: int = 1,
    ) -> NativeLock: ...
    def try_acquire(
        self,
        name: str,
        mode: str,
        ttl: Optional[float],
        namespace: Optional[str],
        capacity: int = 0,
        permits: int = 1,
    ) -> Awaitable[Optional[NativeLock]]: ...
    def try_acquire_sync(
        self,
        name: str,
        mode: str,
        ttl: Optional[float],
        namespace: Optional[str],
        capacity: int = 0,
        permits: int = 1,
    ) -> Optional[NativeLock]: ...
    def close(self) -> Awaitable[None]: ...
    def close_sync(self) -> None: ...
