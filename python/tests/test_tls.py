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
    return make_server(DISTLOCK_TLS_CERT_FILE=str(cert), DISTLOCK_TLS_KEY_FILE=str(key))


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
