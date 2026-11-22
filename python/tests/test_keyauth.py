"""``auth_mode=2``: log in with an Ed25519 key, and the ``min_version`` check."""
from __future__ import annotations

import json
import subprocess
import sys

import pytest

from distlock import (
    AuthenticationError,
    ClientSettings,
    ConfigError,
    DistLockClient,
    ServerTooOld,
    generate_keypair,
    public_key_for,
)


@pytest.fixture(scope="module")
def keys():
    return {name: generate_keypair() for name in ("alice", "other")}


def _users(keys, extra=None):
    doc = {
        "version": 1,
        "users": [
            {"client_id": "alice", "public_keys": [{"key": keys["alice"][1], "identifier": "laptop", "issuer": "tests"}]},
            {"client_id": "old", "public_keys": [{"key": keys["alice"][1], "expiry": "2001-01-01T00:00:00Z"}]},
            *(extra or []),
        ],
    }
    return json.dumps(doc)


def test_a_key_logs_in_and_locks_work(make_server, keys, name):
    srv = make_server(users_json=_users(keys))
    url = f"distlock://alice:{keys['alice'][0]}@127.0.0.1:{srv.port}?auth_mode=v2"
    client = DistLockClient.from_url(url)
    with client.Mutex(name) as guard:
        assert not guard.lost
    assert client.stats_sync().connected_clients == 1


def test_the_wrong_key_an_unknown_user_and_an_expired_key_are_refused(make_server, keys):
    srv = make_server(users_json=_users(keys))
    for user, private in [("alice", keys["other"][0]), ("nobody", keys["alice"][0]), ("old", keys["alice"][0])]:
        client = DistLockClient.from_url(f"distlock://{user}:{private}@127.0.0.1:{srv.port}?auth_mode=2")
        with pytest.raises(AuthenticationError):
            client.ping_sync()


def test_url_params_and_arguments_for_auth_mode_and_min_version():
    private, _ = generate_keypair()
    s = ClientSettings.from_url(f"distlock://a:{private}@h?auth_mode=v2&min_version=0.3.0")
    assert (s.auth_mode, s.min_version) == (2, "0.3.0")
    assert ClientSettings.from_url("distlock://a:b@h").auth_mode == 1
    # Explicit arguments beat the URL.
    s = ClientSettings.from_url(f"distlock://a:{private}@h?auth_mode=1&min_version=0.1.0", auth_mode=2, min_version="9.9.9")
    assert (s.auth_mode, s.min_version) == (2, "9.9.9")
    # The private key is checked up front, and never shown.
    with pytest.raises(ConfigError, match="private key") as e:
        ClientSettings.from_url("distlock://a:not-a-key@h?auth_mode=2")
    assert "not-a-key" not in str(e.value)
    with pytest.raises(ConfigError, match="version"):
        ClientSettings.from_url("distlock://a:b@h?min_version=soon")
    # Standard base64, with padding, is accepted too.
    import base64
    from urllib.parse import quote

    raw = base64.urlsafe_b64decode(private + "=")
    standard = quote(base64.b64encode(raw).decode(), safe="")  # + / = must be percent-encoded in a URL
    assert ClientSettings.from_url(f"distlock://a:{standard}@h?auth_mode=2")


def test_min_version(make_server, keys):
    srv = make_server(users_json=_users(keys))
    base = f"distlock://alice:{keys['alice'][0]}@127.0.0.1:{srv.port}?auth_mode=2"
    assert DistLockClient.from_url(base + "&min_version=0.0.1").ping_sync() >= 0
    with pytest.raises(ServerTooOld, match="99.0.0"):
        DistLockClient.from_url(base + "&min_version=99.0.0").ping_sync()
    with pytest.raises(ServerTooOld):  # an argument beats the URL
        DistLockClient.from_url(base + "&min_version=0.0.1", min_version="99.0.0").ping_sync()


def test_a_server_can_refuse_passwords(make_server, keys):
    srv = make_server(users_json=_users(keys), DISTLOCK_MIN_AUTH_MODE="2")
    assert DistLockClient.from_url(f"distlock://alice:{keys['alice'][0]}@127.0.0.1:{srv.port}?auth_mode=2").ping_sync() >= 0
    with pytest.raises(Exception, match="password logins are disabled"):
        DistLockClient.from_url(f"distlock://alice:whatever@127.0.0.1:{srv.port}").ping_sync()


def test_key_generation():
    private, public = generate_keypair()
    assert len(private) == len(public) == 43 and "=" not in private + public
    assert public_key_for(private) == public
    assert generate_keypair()[0] != private
    with pytest.raises(ValueError):
        public_key_for("short")
    out = subprocess.run(
        [sys.executable, "-m", "distlock.keys", "--client-id", "billing", "--identifier", "laptop", "--expiry", "2030-01-01T00:00:00Z"],
        capture_output=True, text=True, check=True,
    ).stdout
    assert "auth_mode=v2" in out and '"identifier": "laptop"' in out and '"expiry": "2030-01-01T00:00:00Z"' in out
