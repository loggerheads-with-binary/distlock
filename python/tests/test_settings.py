"""URL parsing and settings validation. No server needed."""

from __future__ import annotations

import pytest

from distlock import ClientSettings, ConfigError, DistLockClient

FULL = "distlock://client_id:password@host:1234/namespace?pool_size=5&tls=false&default_ttl=30"


def test_the_documented_url_parses():
    s = ClientSettings.from_url(FULL)
    assert (s.client_id, s.password.get_secret_value(), s.host, s.port) == ("client_id", "password", "host", 1234)
    assert (s.namespace, s.pool_size, s.tls, s.default_ttl) == ("namespace", 5, False, 30.0)


def test_everything_but_credentials_and_host_is_optional():
    s = ClientSettings.from_url("distlock://app:pw@10.0.0.5")
    assert (s.port, s.namespace, s.pool_size, s.tls, s.default_ttl) == (7878, None, 4, False, 30.0)


def test_credentials_may_be_percent_encoded_and_the_password_may_contain_awkward_characters():
    s = ClientSettings.from_url("distlock://my%20app:p%40ss%3Aw%2Frd%25@host")
    assert (s.client_id, s.password.get_secret_value()) == ("my app", "p@ss:w/rd%")
    # An unencoded '@' in the password still works: the last '@' ends the credentials.
    assert ClientSettings.from_url("distlock://app:p@ss@host").password.get_secret_value() == "p@ss"


def test_ipv6_hosts_and_namespace_forms():
    assert ClientSettings.from_url("distlock://a:b@[::1]:9000").host == "::1"
    assert ClientSettings.from_url("distlock://a:b@h/billing/").namespace == "billing"  # trailing slash
    assert ClientSettings.from_url("distlock://a:b@h/my%20app").namespace == "my app"
    assert ClientSettings.from_url("distlock://a:b@h/").namespace is None
    assert ClientSettings.from_url("DISTLOCK://a:b@h").host == "h"  # the scheme is case-insensitive


def test_boolean_and_numeric_parameters_accept_the_usual_spellings():
    s = ClientSettings.from_url("distlock://a:b@h?tls=0&pool_size=08&default_ttl=0.5")
    assert (s.tls, s.pool_size, s.default_ttl) == (False, 8, 0.5)
    assert ClientSettings.from_url("distlock://a:b@h?tls=yes").tls is True


@pytest.mark.parametrize(
    "url, fragment",
    [
        ("", "non-empty"),
        ("http://a:b@h", "scheme"),
        ("a:b@h", "scheme"),
        ("distlock://h", "credentials"),
        ("distlock://onlyuser@h", "credentials"),
        ("distlock://a:b@", "host"),
        ("distlock://a:b@h:99999", "port"),
        ("distlock://a:b@h:notaport", "port"),
        ("distlock://a:b@h#frag", "fragment"),
        ("distlock://a:b@h/one/two", "single segment"),
        ("distlock://a:b@h/we:ird", "':'"),
        ("distlock://a:b@h?mystery=1", "unknown URL parameter"),
        ("distlock://a:b@h?pool_size=1&pool_size=2", "more than once"),
        ("distlock://a:b@h?pool_size", "malformed"),
        ("distlock://a:b@h?pool_size=0", "pool_size"),
        ("distlock://a:b@h?pool_size=1000", "pool_size"),
        ("distlock://a:b@h?pool_size=lots", "pool_size"),
        ("distlock://a:b@h?default_ttl=0", "default_ttl"),
        ("distlock://a:b@h?default_ttl=-3", "default_ttl"),
        ("distlock://a:b@h?default_ttl=soon", "default_ttl"),
        ("distlock://a:b@h?tls=maybe", "tls"),
    ],
)
def test_malformed_urls_are_rejected_with_a_helpful_config_error(url, fragment):
    with pytest.raises(ConfigError) as info:
        ClientSettings.from_url(url)
    assert fragment.lower() in str(info.value).lower(), str(info.value)


def test_a_non_string_url_is_rejected():
    with pytest.raises(ConfigError):
        ClientSettings.from_url(None)  # type: ignore[arg-type]


def test_the_password_never_leaks_through_reprs_or_error_messages():
    secret = "hunter2-very-secret"
    s = ClientSettings.from_url(f"distlock://app:{secret}@h")
    assert secret not in repr(s) and secret not in str(s) and secret not in s.model_dump_json()
    assert secret not in repr(DistLockClient(s))
    # Even when the *password itself* is what is invalid, its value is not echoed.
    with pytest.raises(ConfigError) as info:
        ClientSettings.from_url(f"distlock://app:{secret * 100}@h")
    assert secret not in str(info.value)
    with pytest.raises(ConfigError) as info:
        ClientSettings.from_url(f"distlock://app:{secret}@h?pool_size=0")
    assert secret not in str(info.value)


def test_settings_are_immutable_and_reject_unknown_fields():
    s = ClientSettings.from_url(FULL)
    with pytest.raises(Exception):
        s.port = 1  # type: ignore[misc]
    with pytest.raises(Exception):
        ClientSettings(client_id="a", password="b", host="h", colour="red")  # type: ignore[call-arg]


def test_namespace_cannot_contain_the_separator():
    with pytest.raises(Exception, match="':'"):
        ClientSettings(client_id="a", password="b", host="h", namespace="a:b")
    assert ClientSettings(client_id="a", password="b", host="h", namespace="").namespace is None


def test_the_client_can_be_built_from_keywords_and_validates_them():
    c = DistLockClient(client_id="a", password="b", host="h", port=1, namespace="ns", pool_size=2)
    assert c.settings.namespace == "ns" and c.session_id is None
    with pytest.raises(ConfigError):
        DistLockClient(client_id="", password="b", host="h")
    with pytest.raises(TypeError):
        DistLockClient(ClientSettings.from_url(FULL), client_id="a")


def test_tls_is_refused_up_front_because_the_server_has_none_yet():
    with pytest.raises(ConfigError, match="tls"):
        DistLockClient.from_url("distlock://a:b@h?tls=true")


def test_from_env(monkeypatch):
    monkeypatch.delenv("DISTLOCK_URL", raising=False)
    with pytest.raises(ConfigError, match="DISTLOCK_URL"):
        DistLockClient.from_env()
    monkeypatch.setenv("DISTLOCK_URL", FULL)
    assert DistLockClient.from_env().settings.client_id == "client_id"
    monkeypatch.setenv("OTHER_URL", "distlock://x:y@z")
    assert DistLockClient.from_env("OTHER_URL").settings.host == "z"


def test_creating_a_client_does_not_touch_the_network():
    # Port 1 is closed; nothing may be attempted until a lock is first used.
    client = DistLockClient.from_url("distlock://a:b@127.0.0.1:1")
    lock = client.Mutex("x")
    assert client.session_id is None and lock.name == "x"


def test_locks_validate_their_arguments_when_created():
    client = DistLockClient.from_url("distlock://a:b@127.0.0.1:1")
    for bad in (
        dict(name=""),
        dict(name="x", ttl=0),
        dict(name="x", ttl=-1),
        dict(name="x", namespace="a:b"),
        dict(name="x", wait=-1),
    ):
        with pytest.raises(ValueError):
            client.Mutex(**bad)
    with pytest.raises(ValueError):
        client.Lock("x", mode="sideways")
    with pytest.raises(ConfigError):
        client.Semaphore("x", capacity=0)
    with pytest.raises(ConfigError):
        client.Semaphore("x", capacity=2, permits=3)
