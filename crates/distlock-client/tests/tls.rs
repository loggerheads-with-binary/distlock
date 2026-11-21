//! TLS against a real server: every combination of the two verification flags, against servers whose
//! certificates are right, wrongly named, self-signed, or both wrong.
mod common;
use common::*;
use distlock_client::{Acquire, DistLockClient, Error};
use std::path::Path;
fn client(srv: &TestServer, ca: &Path, verify_ca: bool, verify_host: bool) -> DistLockClient {
    DistLockClient {
        tls: true,
        tls_verify_ca: verify_ca,
        tls_verify_host: verify_host,
        ca_file_path: Some(ca.to_owned()),
        pool_size: 1,
        ..srv.client("billing")
    }
}
#[tokio::test]
async fn each_mode_accepts_and_rejects_the_right_certificates() {
    let pki = Pki::new();
    let good = tls_server(&pki.issued("good", names("localhost"))).await;
    let wrong_name = tls_server(&pki.issued("wrong-name", wrong_names())).await;
    let self_signed = tls_server(&pki.self_signed("self-signed", names("localhost"))).await;
    let both_wrong = tls_server(&pki.self_signed("both-wrong", wrong_names())).await;
    // (verify_ca, verify_host) -> passes against: good, wrong name, self-signed, both wrong
    let table = [
        ((true, true), [true, false, false, false], "full"),
        ((true, false), [true, true, false, false], "CA only"),
        ((false, true), [true, false, true, false], "name only"),
        ((false, false), [true, true, true, true], "none"),
    ];
    for ((ca, host), expected, mode) in table {
        for (srv, (want, what)) in [&good, &wrong_name, &self_signed, &both_wrong]
            .into_iter()
            .zip(
                expected
                    .into_iter()
                    .zip(["good", "wrong name", "self-signed", "both wrong"]),
            )
        {
            let got = client(srv, &pki.ca_file(), ca, host).connect().await;
            match (want, got) {
                (true, Ok(c)) => {
                    // Not just connected: the whole protocol works over the encrypted stream.
                    let lock = c
                        .acquire(Acquire::mutex(format!("m-{mode}-{what}")))
                        .await
                        .unwrap();
                    lock.release().await.unwrap();
                }
                (false, Err(Error::Tls(m))) => {
                    assert!(m.contains("certificate"), "{mode}/{what}: {m}")
                }
                (want, got) => panic!(
                    "{mode} against {what}: expected ok={want}, got {:?}",
                    got.map(|_| ())
                ),
            }
        }
    }
}
#[tokio::test]
async fn a_client_without_the_ca_rejects_a_private_ca_server() {
    let pki = Pki::new();
    let srv = tls_server(&pki.issued("good", names("localhost"))).await;
    // No ca_file: only the public roots are trusted, and this CA is not one of them.
    let c = DistLockClient {
        tls: true,
        pool_size: 1,
        ..srv.client("billing")
    };
    assert!(matches!(c.connect().await, Err(Error::Tls(_))));
}
#[tokio::test]
async fn tls_and_plaintext_do_not_mix() {
    let pki = Pki::new();
    let tls = tls_server(&pki.issued("good", names("localhost"))).await;
    let plain = TestServer::start().await;
    let plaintext_client = DistLockClient {
        pool_size: 1,
        ..tls.client("billing")
    };
    assert!(plaintext_client.connect().await.is_err());
    let tls_client = client(&plain, &pki.ca_file(), true, true);
    assert!(tls_client.connect().await.is_err());
}
#[tokio::test]
async fn bad_client_settings_are_refused_before_connecting() {
    let pki = Pki::new();
    let srv = TestServer::start().await;
    let missing = DistLockClient {
        ca_file_path: Some(pki.dir.path().join("nope.pem")),
        ..client(&srv, &pki.ca_file(), true, true)
    };
    assert!(matches!(missing.connect().await, Err(Error::Tls(_))));
    let empty = pki.dir.path().join("empty.pem");
    std::fs::write(&empty, "").unwrap();
    let c = DistLockClient {
        ca_file_path: Some(empty),
        ..client(&srv, &pki.ca_file(), true, true)
    };
    assert!(matches!(c.connect().await, Err(Error::Tls(_))));
}

#[tokio::test]
async fn the_ca_can_be_given_as_text_and_only_one_source_is_allowed() {
    let pki = Pki::new();
    let srv = tls_server(&pki.issued("good", names("localhost"))).await;
    let text = DistLockClient {
        ca_file_path: None,
        ca_file_content: Some(std::fs::read_to_string(pki.ca_file()).unwrap()),
        ..client(&srv, &pki.ca_file(), true, true)
    };
    assert!(text.connect().await.is_ok());
    let junk = DistLockClient {
        ca_file_path: None,
        ca_file_content: Some("not a certificate".into()),
        ..client(&srv, &pki.ca_file(), true, true)
    };
    assert!(matches!(junk.connect().await, Err(Error::Tls(m)) if m.contains("ca_file_content")));
    let both = DistLockClient {
        ca_file_content: Some("x".into()),
        ..client(&srv, &pki.ca_file(), true, true)
    };
    assert!(matches!(both.connect().await, Err(Error::Config(_))));
}
