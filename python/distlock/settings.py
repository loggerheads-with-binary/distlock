"""Client settings, validated by pydantic, and the ``distlock://`` URL form."""

from __future__ import annotations

import re
from typing import Any, Dict, List, Optional, Tuple
from urllib.parse import parse_qsl, unquote, urlsplit

from pydantic import BaseModel, ConfigDict, Field, SecretStr, ValidationError, field_validator, model_validator

from .errors import ConfigError

_SCHEME = "distlock"
_QUERY_PARAMETERS = ("pool_size", "tls", "tls_verify_ca", "tls_verify_host", "ca_file_path", "ca_file_content", "auth_mode", "min_version", "default_ttl")
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
    #: Connect over TLS. The server must have a certificate configured.
    tls: bool = False
    #: With ``tls``: require the server certificate to chain to a trusted CA (``ca_file_path``, or the
    #: built-in public roots).
    tls_verify_ca: bool = True
    #: With ``tls``: require the server certificate to name ``host`` (a DNS name or an IP address).
    #: The two flags give four modes: both on is full verification; ``tls_verify_host`` off is
    #: "CA only"; ``tls_verify_ca`` off is "name only" (a self-signed certificate passes); both off
    #: encrypts without authenticating the server, which stops passive eavesdropping only.
    tls_verify_host: bool = True
    #: PEM file of CA certificates to trust instead of the built-in public roots. Set this or
    #: ``ca_file_content``, not both.
    ca_file_path: Optional[str] = Field(default=None, min_length=1)
    #: The same CA certificates as PEM *text*, for when there is no file (a secret from a vault or an
    #: environment variable).
    ca_file_content: Optional[str] = Field(default=None, min_length=1)
    #: How to log in. ``1``: send ``password`` (checked against a bcrypt hash; without TLS it crosses
    #: the network in the clear). ``2``: prove possession of an Ed25519 private key without sending
    #: it; ``password`` then holds the 32-byte private key in base64 (43 characters URL-safe). The
    #: server must list the matching public key for ``client_id``. See ``distlock.keys``.
    auth_mode: int = Field(default=1, ge=1)
    #: The oldest server release accepted, like ``"0.3.0"``. Checked right after logging in. ``None``:
    #: the release this client was built to work with.
    min_version: Optional[str] = None
    #: Lease length, in seconds, for locks that do not set their own ``ttl``.
    default_ttl: float = Field(default=30.0, gt=0, le=_MAX_TTL_SECONDS)

    @model_validator(mode="after")
    def _one_ca_source(self) -> "ClientSettings":
        if self.ca_file_path is not None and self.ca_file_content is not None:
            raise ValueError("set only one of ca_file_path and ca_file_content")
        return self

    @model_validator(mode="after")
    def _key_login_needs_a_key(self) -> "ClientSettings":
        if self.auth_mode == 2 and _key32(self.password.get_secret_value()) is None:
            raise ValueError("auth_mode 2 needs the private key (32 bytes in base64) as the password")
        return self

    @field_validator("min_version")
    @classmethod
    def _min_version(cls, value: Optional[str]) -> Optional[str]:
        if value is not None and not re.fullmatch(r"[vV]?\d+\.\d+(\.\d+)?([-+][0-9A-Za-z.+-]*)?", value.strip()):
            raise ValueError("must be a version like 0.3.0")
        return value

    @field_validator("namespace")
    @classmethod
    def _namespace(cls, value: Optional[str]) -> Optional[str]:
        if value is None or value == "":
            return None
        if ":" in value:
            raise ValueError("must not contain ':' (it separates the namespace from the lock name)")
        return value

    @classmethod
    def from_url(
        cls,
        url: str,
        *,
        tls: Optional[bool] = None,
        tls_verify_ca: Optional[bool] = None,
        tls_verify_host: Optional[bool] = None,
        ca_file_path: Optional[str] = None,
        ca_file_content: Optional[str] = None,
        auth_mode: Optional[int] = None,
        min_version: Optional[str] = None,
    ) -> "ClientSettings":
        """Parse ``distlock://client_id:password@host[:port][/namespace][?pool_size=&tls=&tls_verify_ca=&tls_verify_host=&ca_file_path=&ca_file_content=&auth_mode=&min_version=&default_ttl=]``.

        User and password may be percent-encoded. The path, if any, is the namespace (one segment).
        ``default_ttl`` is in seconds. Unknown query parameters are an error, not ignored.

        The keyword arguments are the TLS settings. One that is given (not ``None``) **overrides** the
        same setting in the URL; one left out leaves the URL's value. Giving ``ca_file_path`` replaces a
        ``ca_file_content`` from the URL, and the other way round. Giving both is an error.

        Raises :class:`~distlock.errors.ConfigError` if the URL is malformed or a value is invalid.
        """
        fields = _parse_url(url)
        explicit = {
            "tls": tls,
            "tls_verify_ca": tls_verify_ca,
            "tls_verify_host": tls_verify_host,
            "ca_file_path": ca_file_path,
            "ca_file_content": ca_file_content,
            "auth_mode": auth_mode,
            "min_version": min_version,
        }
        if ca_file_path is not None and ca_file_content is not None:
            raise ConfigError("set only one of ca_file_path and ca_file_content")
        if ca_file_path is not None:
            fields.pop("ca_file_content", None)
        if ca_file_content is not None:
            fields.pop("ca_file_path", None)
        fields.update({k: v for k, v in explicit.items() if v is not None})
        try:
            return cls.model_validate(fields)
        except ValidationError as e:
            raise ConfigError(f"invalid distlock URL: {validation_message(e)}") from None


def _key32(text: str) -> Optional[bytes]:
    """The 32 bytes behind a base64 (standard or URL-safe, padded or not) key, or ``None``."""
    import base64
    import binascii

    cleaned = text.strip().rstrip("=").replace("+", "-").replace("/", "_")
    try:
        raw = base64.urlsafe_b64decode(cleaned + "=" * (-len(cleaned) % 4))
    except (binascii.Error, ValueError):
        return None
    return raw if len(raw) == 32 else None


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
        fields[key] = value.lstrip("vV") if key == "auth_mode" else value  # v2 means 2
    return fields
