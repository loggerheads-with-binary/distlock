"""Client settings, validated by pydantic, and the ``distlock://`` URL form."""

from __future__ import annotations

from typing import Any, Dict, List, Optional, Tuple
from urllib.parse import parse_qsl, unquote, urlsplit

from pydantic import BaseModel, ConfigDict, Field, SecretStr, ValidationError, field_validator

from .errors import ConfigError

_SCHEME = "distlock"
_QUERY_PARAMETERS = ("pool_size", "tls", "default_ttl")
#: The server counts leases in 32-bit milliseconds.
_MAX_TTL_SECONDS = 4_000_000.0


def validation_message(error: ValidationError) -> str:
    """Pydantic's complaints as one line, without echoing any input (it may be a password)."""
    return "; ".join(
        f"{'.'.join(str(p) for p in e['loc']) or 'settings'}: {e['msg']}" for e in error.errors(include_input=False)
    )


class ClientSettings(BaseModel):
    """Everything needed to reach the server and name locks.

    Build one directly, or from a URL::

        ClientSettings.from_url("distlock://billing:s3cret@10.0.0.5:7878/billing?pool_size=5&default_ttl=30")
    """

    model_config = ConfigDict(frozen=True, extra="forbid", hide_input_in_errors=True, str_strip_whitespace=True)

    #: Login name, and the application's name on the server. Many processes may share one.
    client_id: str = Field(min_length=1, max_length=128)
    password: SecretStr = Field(max_length=1024)
    host: str = Field(min_length=1)
    port: int = Field(default=7878, ge=1, le=65535)
    #: Prefix applied to every lock name (``namespace:...``), to keep applications apart.
    namespace: Optional[str] = None
    #: TCP connections in the pool. They all belong to one session.
    pool_size: int = Field(default=4, ge=1, le=256)
    #: TLS is not supported by the server yet; ``True`` is rejected when connecting.
    tls: bool = False
    #: Lease length, in seconds, for locks that do not set their own ``ttl``.
    default_ttl: float = Field(default=30.0, gt=0, le=_MAX_TTL_SECONDS)

    @field_validator("namespace")
    @classmethod
    def _namespace(cls, value: Optional[str]) -> Optional[str]:
        if value is None or value == "":
            return None
        if ":" in value:
            raise ValueError("must not contain ':' (it separates the namespace from the lock name)")
        return value

    @classmethod
    def from_url(cls, url: str) -> "ClientSettings":
        """Parse ``distlock://client_id:password@host[:port][/namespace][?pool_size=&tls=&default_ttl=]``.

        User and password may be percent-encoded. The path, if any, is the namespace (one segment).
        ``default_ttl`` is in seconds. Unknown query parameters are an error, not ignored.

        Raises :class:`~distlock.errors.ConfigError` if the URL is malformed or a value is invalid.
        """
        fields = _parse_url(url)
        try:
            return cls.model_validate(fields)
        except ValidationError as e:
            raise ConfigError(f"invalid distlock URL: {validation_message(e)}") from None


def _parse_url(url: str) -> Dict[str, Any]:
    if not isinstance(url, str) or not url.strip():
        raise ConfigError("the distlock URL must be a non-empty string")
    try:
        parts = urlsplit(url.strip())
        port = parts.port  # raises ValueError for a non-numeric or out-of-range port
    except ValueError as e:
        raise ConfigError(f"malformed distlock URL: {e}") from None
    if parts.scheme.lower() != _SCHEME:
        raise ConfigError(f"unsupported URL scheme {parts.scheme!r}: expected '{_SCHEME}://'")
    if parts.fragment:
        raise ConfigError("a distlock URL cannot have a #fragment")
    if parts.username is None or parts.password is None:
        raise ConfigError(f"the URL must carry credentials: {_SCHEME}://client_id:password@host")
    host = parts.hostname
    if not host:
        raise ConfigError("the URL must name a host")

    fields: Dict[str, Any] = {
        "client_id": unquote(parts.username),
        "password": unquote(parts.password),
        "host": host,
    }
    if port is not None:
        fields["port"] = port

    segment = unquote(parts.path[1:] if parts.path.startswith("/") else parts.path)
    if segment.endswith("/"):
        segment = segment[:-1]
    if "/" in segment:
        raise ConfigError("the URL path is the namespace and must be a single segment (no '/')")
    if segment:
        fields["namespace"] = segment

    try:
        pairs: List[Tuple[str, str]] = parse_qsl(parts.query, keep_blank_values=True, strict_parsing=bool(parts.query))
    except ValueError as e:
        raise ConfigError(f"malformed query string: {e}") from None
    seen = set()
    for key, value in pairs:
        if key not in _QUERY_PARAMETERS:
            raise ConfigError(f"unknown URL parameter {key!r}: expected one of {', '.join(_QUERY_PARAMETERS)}")
        if key in seen:
            raise ConfigError(f"URL parameter {key!r} appears more than once")
        seen.add(key)
        fields[key] = value
    return fields
