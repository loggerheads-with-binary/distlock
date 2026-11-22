"""Ed25519 keys for ``auth_mode=2`` logins.

Make a pair, put the public half in the server's ``users.json`` and the private half in the client::

    python -m distlock.keys --client-id billing --identifier laptop

    private, public = generate_keypair()

Needs the ``cryptography`` package (``pip install "pydistlock[keys]"``). Both halves are 32 bytes,
written as URL-safe base64 without padding (43 characters), so they can go into a URL as they are.
The private half is a secret: keep it out of source control and logs, and prefer a file or an
environment variable over typing it on a command line.
"""
from __future__ import annotations

import argparse
import base64
import json
from typing import Optional, Sequence, Tuple


def _crypto():
    try:
        from cryptography.hazmat.primitives import serialization
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    except ImportError:
        raise ImportError(
            "making keys needs the 'cryptography' package: pip install 'pydistlock[keys]'"
        ) from None
    return serialization, Ed25519PrivateKey


def _b64(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).decode().rstrip("=")


def generate_keypair() -> Tuple[str, str]:
    """A new ``(private_key, public_key)``, each 43 characters of URL-safe base64."""
    serialization, Ed25519PrivateKey = _crypto()
    key = Ed25519PrivateKey.generate()
    raw = serialization.Encoding.Raw
    private = key.private_bytes(raw, serialization.PrivateFormat.Raw, serialization.NoEncryption())
    public = key.public_key().public_bytes(raw, serialization.PublicFormat.Raw)
    return _b64(private), _b64(public)


def public_key_for(private_key: str) -> str:
    """The public key that goes with a private key (so a lost ``users.json`` entry can be remade)."""
    from .settings import _key32

    serialization, Ed25519PrivateKey = _crypto()
    seed = _key32(private_key)
    if seed is None:
        raise ValueError("not a private key: expected 32 bytes in base64")
    public = Ed25519PrivateKey.from_private_bytes(seed).public_key()
    return _b64(public.public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw))


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        prog="python -m distlock.keys", description="Make an Ed25519 key pair for auth_mode=2 logins."
    )
    parser.add_argument("--client-id", default="CLIENT_ID", help="the user the key is for (only used in the printed entry)")
    parser.add_argument("--identifier", help="a name for this key in the server log, e.g. laptop")
    parser.add_argument("--issuer", help="metadata only: who vouched for the key")
    parser.add_argument("--expiry", help="RFC 3339 time after which the key stops working, e.g. 2026-01-31T00:00:00Z")
    args = parser.parse_args(argv)

    private, public = generate_keypair()
    entry = {"key": public}
    for name in ("identifier", "issuer", "expiry"):
        if getattr(args, name):
            entry[name] = getattr(args, name)
    print("private key (SECRET: the client's password in an auth_mode=2 URL):")
    print(f"  {private}")
    print("public key:")
    print(f"  {public}")
    print(f'\nadd to the user in users.json, under "public_keys":\n  {json.dumps(entry)}')
    print(f"\nclient URL:\n  distlock://{args.client_id}:{private}@HOST:7878?auth_mode=v2")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
