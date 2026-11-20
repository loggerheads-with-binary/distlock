//! TLS between the real server and the real client, with a throwaway self-signed certificate for
//! 127.0.0.1 (clients reach the server by IP address).
mod common;
use common::*;
use distlock_client::{Acquire, DistLockClient, Error};

struct Pki {
    _dir: tempfile::TempDir,
    cert: std::path::PathBuf,
    key: std::path::PathBuf,
}

fn pki() -> Pki {
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (c, k) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&c, cert.cert.pem()).unwrap();
    std::fs::write(&k, cert.signing_key.serialize_pem()).unwrap();
    Pki {
        _dir: dir,
        cert: c,
        key: k,
    }
}

async fn tls_server(pki: &Pki) -> TestServer {
    let (c, k) = (pki.cert.clone(), pki.key.clone());
    TestServer::start_with("127.0.0.1:0".parse().unwrap(), move |cfg| {
        cfg.tls_cert_file = Some(c);
        cfg.tls_key_file = Some(k);
    })
    .await
}

async fn lock_roundtrip(client: DistLockClient) {
    let client = client.connect().await.unwrap();
    let lock = client.acquire(Acquire::mutex("tls")).await.unwrap();
    lock.release().await.unwrap();
}

#[tokio::test]
async fn works_without_verification() {
    let pki = pki();
    let srv = tls_server(&pki).await;
    let client = DistLockClient {
        tls: true,
        tls_verify: false,
        ..srv.client("billing")
    };
    lock_roundtrip(client).await;
    srv.stop().await;
}

#[tokio::test]
async fn works_with_the_certificate_as_trust_anchor() {
    let pki = pki();
    let srv = tls_server(&pki).await;
    let client = DistLockClient {
        tls: true,
        tls_ca_file: Some(pki.cert.clone()),
        ..srv.client("billing")
    };
    lock_roundtrip(client).await;
    srv.stop().await;
}

#[tokio::test]
async fn an_untrusted_certificate_is_refused_when_verifying() {
    let pki = pki();
    let srv = tls_server(&pki).await;
    let client = DistLockClient {
        tls: true,
        ..srv.client("billing")
    };
    match client.connect().await {
        Err(Error::Connect { reason, .. }) => assert!(reason.contains("TLS"), "{reason}"),
        other => panic!("expected a TLS connect failure, got {other:?}"),
    }
    srv.stop().await;
}

#[tokio::test]
async fn a_plaintext_client_cannot_use_a_tls_server() {
    let pki = pki();
    let srv = tls_server(&pki).await;
    assert!(srv.client("billing").connect().await.is_err());
    srv.stop().await;
}
