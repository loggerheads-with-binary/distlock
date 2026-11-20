"""distlock: distributed locks with leases, backed by a Rust client.

    from distlock import DistLockClient, ProcessingStatus

    client = DistLockClient.from_url("distlock://billing:s3cret@10.0.0.5:7878/billing?pool_size=5&default_ttl=30")

    async with client.mutex("nightly-report") as guard:
        if guard.status is ProcessingStatus.SUCCEEDED:
            return
        ...
        guard.set_status(ProcessingStatus.SUCCEEDED)
"""

from ._native import __version__
from .client import DistLockClient
from .enums import LockMode, PreviousStatus, ProcessingStatus
from .errors import (
    AuthenticationError,
    ClientClosed,
    ConfigError,
    ConnectionFailed,
    DistLockError,
    InvalidArgument,
    LockBusy,
    LockLost,
    LockTimeout,
    MaintenanceError,
    ServerError,
    SessionLost,
)
from .event import Event
from .lock import Lock, LockGuard
from .maintenance import Maintenance, ResetUsersReport
from .settings import ClientSettings

__all__ = [
    "__version__",
    "DistLockClient",
    "ClientSettings",
    "Lock",
    "LockGuard",
    "Event",
    "LockMode",
    "ProcessingStatus",
    "PreviousStatus",
    "Maintenance",
    "ResetUsersReport",
    # errors
    "DistLockError",
    "InvalidArgument",
    "ConfigError",
    "AuthenticationError",
    "ConnectionFailed",
    "SessionLost",
    "LockTimeout",
    "LockBusy",
    "LockLost",
    "MaintenanceError",
    "ClientClosed",
    "ServerError",
]
