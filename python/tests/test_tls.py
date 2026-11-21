"""TLS: the server with a certificate, and the client's two verification flags (four modes)."""
from __future__ import annotations

import datetime
import ipaddress
from pathlib import Path

import pytest
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import NameOID

from distlock import DistLockClient, TlsError


def _name(cn: str) -> x509.Name:
    return x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, cn)])


def _cert(subject_key, issuer_key, issuer_name, subject_name, sans, ca=False):
    now = datetime.datetime.now(datetime.timezone.utc)
    builder = (
        x509.CertificateBuilder()
        .subject_name(subject_name)
        .issuer_name(issuer_name)
        .public_key(subject_key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(minutes=5))
        .not_valid_after(now + datetime.timedelta(days=1))
        .add_extension(x509.BasicConstraints(ca=ca, path_length=None), critical=True)
    )
    if sans:
        builder = builder.add_extension(x509.SubjectAlternativeName(sans), critical=False)
    return builder.sign(issuer_key, hashes.SHA256())


def _write(path: Path, cert=None, key=None) -> Path:
    if cert is not None:
        path.write_bytes(cert.public_bytes(serialization.Encoding.PEM))
    if key is not None:
        path.write_bytes(
            key.private_bytes(
                serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()
            )
        )
    return path


@pytest.fixture(scope="module")
def pki(tmp_path_factory):
    d = tmp_path_factory.mktemp("pki")
    ca_key = ec.generate_private_key(ec.SECP256R1())
    ca = _cert(ca_key, ca_key, _name("test ca"), _name("test ca"), None, ca=True)
    ca_file = _write(d / "ca.pem", ca)

    def issued(label: str, sans):
        key = ec.generate_private_key(ec.SECP256R1())
        cert = _cert(key, ca_key, ca.subject, _name(label), sans)
        return _write(d / f"{label}.crt", cert), _write(d / f"{label}.key", key=key)

    good = [x509.IPAddress(ipaddress.ip_address("127.0.0.1"))]
    wrong = [x509.DNSName("wrong.example")]
    return {"ca": ca_file, "good": issued("good", good), "wrong": issued("wrong", wrong)}


def _server(make_server, pair):
    cert, key = pair
    return make_server(DISTLOCK_TLS_ENABLED="true", DISTLOCK_TLS_CERT_FILE=str(cert), DISTLOCK_TLS_KEY_FILE=str(key))


def test_full_verification_works_and_the_protocol_runs_over_tls(make_server, pki, name):
    srv = _server(make_server, pki["good"])
    client = srv.client(tls="true", ca_file_path=pki["ca"])
    with client.Mutex(name) as guard:
        assert not guard.lost
    assert client.stats_sync().connected_clients == 1


@pytest.mark.parametrize(
    "flags, passes",
    [
        ({}, False),
        ({"tls_verify_host": "false"}, True),  # CA only
        ({"tls_verify_ca": "false"}, False),  # name only; the name is wrong
        ({"tls_verify_ca": "false", "tls_verify_host": "false"}, True),  # none
    ],
)
def test_the_two_flags_against_a_certificate_with_the_wrong_name(make_server, pki, flags, passes):
    srv = _server(make_server, pki["wrong"])
    client = srv.client(tls="true", ca_file_path=pki["ca"], **flags)
    if passes:
        assert client.ping_sync() >= 0
    else:
        with pytest.raises(TlsError, match="certificate"):
            client.ping_sync()


def test_a_client_that_does_not_trust_the_ca_is_refused(make_server, pki):
    srv = _server(make_server, pki["good"])
    with pytest.raises(TlsError):
        srv.client(tls="true").ping_sync()


def test_tls_and_plaintext_do_not_mix(make_server, pki, server):
    tls_server = _server(make_server, pki["good"])
    with pytest.raises(Exception):
        tls_server.client().ping_sync()
    with pytest.raises(Exception):
        server.client(tls="true", ca_file_path=pki["ca"]).ping_sync()


def test_a_bad_ca_file_is_a_tls_error(make_server, pki, tmp_path):
    srv = _server(make_server, pki["good"])
    with pytest.raises(TlsError, match="CA file"):
        srv.client(tls="true", ca_file_path=tmp_path / "missing.pem").ping_sync()


def test_the_ca_can_be_given_as_text_instead_of_a_file(make_server, pki):
    srv = _server(make_server, pki["good"])
    client = DistLockClient(
        client_id="billing", password="pw-billing", host="127.0.0.1", port=srv.port, tls=True,
        ca_file_content=pki["ca"].read_text(),
    )
    assert client.ping_sync() >= 0
    with pytest.raises(TlsError, match="ca_file_content"):
        DistLockClient(
            client_id="billing", password="pw-billing", host="127.0.0.1", port=srv.port, tls=True,
            ca_file_content="not a certificate",
        ).ping_sync()


def test_the_server_exits_when_tls_is_enabled_without_a_usable_certificate(make_server, pki, tmp_path):
    missing = tmp_path / "missing.pem"
    with pytest.raises(RuntimeError, match="server exited early"):
        make_server(DISTLOCK_TLS_ENABLED="true", DISTLOCK_TLS_CERT_FILE=str(missing), DISTLOCK_TLS_KEY_FILE=str(missing))
    with pytest.raises(RuntimeError, match="DISTLOCK_TLS_ENABLED"):
        make_server(DISTLOCK_TLS_ENABLED="true")
    cert, key = pki["good"]
    with pytest.raises(RuntimeError, match="DISTLOCK_TLS_ENABLED"):  # files without the flag
        make_server(DISTLOCK_TLS_CERT_FILE=str(cert), DISTLOCK_TLS_KEY_FILE=str(key))


# ------------------------------------------------------------------ the scripts in scripts/tls
import subprocess  # noqa: E402

from conftest import REPO  # noqa: E402

TLS_SCRIPTS = REPO / "scripts" / "tls"


def _run(script: str, *args, env=None, ok=True):
    out = subprocess.run(
        [str(TLS_SCRIPTS / script), *map(str, args)], capture_output=True, text=True, env={**__import__("os").environ, **(env or {})}
    )
    assert (out.returncode == 0) == ok, out.stdout + out.stderr
    return out


def _tls_server(make_server, d: Path):
    return make_server(
        DISTLOCK_TLS_ENABLED="true", DISTLOCK_TLS_CERT_FILE=str(d / "server.crt"), DISTLOCK_TLS_KEY_FILE=str(d / "server.key")
    )


def test_new_ca_and_server_cert_give_a_certificate_that_full_verification_accepts(make_server, tmp_path):
    _run("new-ca.sh", "--dir", tmp_path)
    _run("server-cert.sh", "--dir", tmp_path, "--name", "localhost", "--ip", "127.0.0.1")
    srv = _tls_server(make_server, tmp_path)
    assert srv.client(tls="true", ca_file_path=tmp_path / "ca.crt").ping_sync() >= 0
    # A certificate for another name fails the name check but passes "CA only".
    _run("server-cert.sh", "--dir", tmp_path, "--name", "other.example", "--force")
    other = _tls_server(make_server, tmp_path)
    with pytest.raises(TlsError, match="certificate"):
        other.client(tls="true", ca_file_path=tmp_path / "ca.crt").ping_sync()
    assert other.client(tls="true", ca_file_path=tmp_path / "ca.crt", tls_verify_host="false").ping_sync() >= 0


def test_the_scripts_never_overwrite_without_force(tmp_path):
    _run("new-ca.sh", "--dir", tmp_path)
    _run("server-cert.sh", "--dir", tmp_path, "--ip", "127.0.0.1")
    for script, args in (("new-ca.sh", []), ("server-cert.sh", ["--ip", "127.0.0.1"])):
        out = _run(script, "--dir", tmp_path, *args, ok=False)
        assert "--force" in out.stderr
    _run("server-cert.sh", "--dir", tmp_path, "--name", "x.example", ok=False, env={})  # server.key exists
    _run("server-cert.sh", "--dir", tmp_path, ok=False)  # no name or IP


def test_an_existing_ca_and_an_existing_encrypted_key(make_server, tmp_path):
    ca, mine = tmp_path / "company-ca", tmp_path / "mine"
    _run("new-ca.sh", "--dir", ca)
    _run("use-ca.sh", "--cert", ca / "ca.crt", "--key", ca / "ca.key", "--dir", mine)
    encrypted = tmp_path / "old.key"
    subprocess.run(
        ["openssl", "genpkey", "-algorithm", "EC", "-pkeyopt", "ec_paramgen_curve:prime256v1",
         "-pkeyopt", "ec_param_enc:named_curve", "-aes256", "-pass", "pass:hunter2", "-out", str(encrypted)],
        check=True, capture_output=True,
    )
    _run("server-cert.sh", "--dir", mine, "--ip", "127.0.0.1", "--key", encrypted, ok=False, env={"TLS_KEY_PASSPHRASE": "wrong"})
    _run("server-cert.sh", "--dir", mine, "--ip", "127.0.0.1", "--key", encrypted, env={"TLS_KEY_PASSPHRASE": "hunter2"})
    assert "ENCRYPTED" not in (mine / "server.key").read_text(), "the server cannot ask for a passphrase"
    srv = _tls_server(make_server, mine)
    assert srv.client(tls="true", ca_file_path=ca / "ca.crt").ping_sync() >= 0


def test_use_ca_rejects_what_cannot_be_a_ca(tmp_path):
    _run("new-ca.sh", "--dir", tmp_path / "a")
    _run("new-ca.sh", "--dir", tmp_path / "b")
    _run("server-cert.sh", "--dir", tmp_path / "a", "--ip", "127.0.0.1")
    out = _run("use-ca.sh", "--cert", tmp_path / "a" / "server.crt", "--dir", tmp_path / "x", ok=False)
    assert "not a CA" in out.stderr
    out = _run("use-ca.sh", "--cert", tmp_path / "a" / "ca.crt", "--key", tmp_path / "b" / "ca.key", "--dir", tmp_path / "x", ok=False)
    assert "not the private key" in out.stderr
    _run("use-ca.sh", "--cert", tmp_path / "a" / "ca.crt", "--dir", tmp_path / "y")  # certificate only is fine
    out = _run("server-cert.sh", "--dir", tmp_path / "y", "--ip", "127.0.0.1")
    assert "server.csr" in out.stdout and not (tmp_path / "y" / "server.crt").exists()


def _sans(cert: Path) -> list:
    out = subprocess.run(["openssl", "x509", "-in", str(cert), "-noout", "-text"], capture_output=True, text=True, check=True).stdout
    line = out.split("Subject Alternative Name:")[1].splitlines()[1]
    return sorted(part.strip().replace("IP Address:", "IP:") for part in line.split(","))


def test_one_certificate_can_hold_many_names_a_wildcard_and_many_addresses(make_server, tmp_path):
    _run("new-ca.sh", "--dir", tmp_path)
    out = _run(
        "server-cert.sh", "--dir", tmp_path,
        "--name", "a.example, b.example", "--name", "*.svc.example", "--name", "a.example",  # comma list, repeat, duplicate
        "--ip", "127.0.0.1,10.0.0.5", "--ip", "::1",
    )
    assert _sans(tmp_path / "server.crt") == sorted(
        ["DNS:a.example", "DNS:b.example", "DNS:*.svc.example", "IP:127.0.0.1", "IP:10.0.0.5", "IP:0:0:0:0:0:0:0:1"]
    )
    assert "DNS:a.example,DNS:b.example" in out.stdout
    # The real client, connecting to one of the listed addresses, passes full verification.
    srv = _tls_server(make_server, tmp_path)
    assert srv.client(tls="true", ca_file_path=tmp_path / "ca.crt").ping_sync() >= 0


@pytest.mark.parametrize(
    "args, message",
    [
        (["--name", "*.com"], "not a valid host name"),  # would cover a whole TLD
        (["--name", "a*.example.com"], "not a valid host name"),  # * must be the whole label
        (["--name", "foo.*.example.com"], "not a valid host name"),  # and the left-most one
        (["--name", "*"], "not a valid host name"),
        (["--name", "bad_name.example"], "not a valid host name"),
        (["--ip", "999.1.1.1"], "not a valid IP"),
        (["--ip", "10.0.0.0/24"], "not supported"),  # a certificate cannot hold a range
    ],
)
def test_names_and_addresses_that_cannot_be_in_a_certificate_are_rejected(tmp_path, args, message):
    _run("new-ca.sh", "--dir", tmp_path)
    out = _run("server-cert.sh", "--dir", tmp_path, *args, ok=False)
    assert message in out.stderr and not (tmp_path / "server.crt").exists()
