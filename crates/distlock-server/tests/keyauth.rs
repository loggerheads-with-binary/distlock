//! Auth mode 2 on the wire: the nonce challenge, replays, and what a refusal looks like.
mod common;
use common::*;
use distlock_proto::v1 as pb;
use distlock_proto::v1::{client_frame as cf, server_frame as sf};
use distlock_server::UserDb;
use pb::ErrorCode;
use ring::signature::{Ed25519KeyPair, KeyPair};

fn pair() -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(&[42; 32]).unwrap()
}
async fn server() -> TestServer {
    let key: [u8; 32] = pair().public_key().as_ref().try_into().unwrap();
    TestServer::start_with_users(
        UserDb::for_tests_with_keys(&[("alice", &[key], true)]),
        |_| {},
    )
    .await
}
fn start(user: &str) -> cf::Body {
    cf::Body::AuthStart(pb::AuthStart {
        protocol_version: 1,
        auth_mode: 2,
        client_id: user.into(),
        instance_label: "test".into(),
    })
}
fn proof(signature: &[u8]) -> cf::Body {
    cf::Body::AuthProof(pb::AuthProof {
        signature: signature.to_vec(),
    })
}
fn sign(nonce: &[u8], user: &str) -> Vec<u8> {
    pair()
        .sign(&distlock_proto::auth_message(nonce, user))
        .as_ref()
        .to_vec()
}
async fn challenge(c: &mut Client, user: &str) -> Vec<u8> {
    match c.call(start(user)).await {
        sf::Body::AuthChallenge(ch) => ch.nonce,
        other => panic!("expected AuthChallenge, got {other:?}"),
    }
}

#[tokio::test]
async fn the_signature_of_the_nonce_opens_a_session_and_the_reply_reports_the_server() {
    let srv = server().await;
    let mut c = Client::connect(srv.addr).await;
    let nonce = challenge(&mut c, "alice").await;
    assert_eq!(nonce.len(), 32);
    // AuthProof reuses the seq of AuthStart.
    c.send_with_seq(1, proof(&sign(&nonce, "alice"))).await;
    let Recv::Frame(f) = c.recv(WAIT).await else {
        panic!("no reply")
    };
    assert_eq!(f.seq, 1);
    let Some(sf::Body::HelloOk(ok)) = f.body else {
        panic!("expected HelloOk, got {f:?}")
    };
    assert_ne!(ok.session_id, 0);
    assert_eq!(ok.server_version, env!("CARGO_PKG_VERSION"));
    assert_eq!((ok.min_auth_mode, ok.max_auth_mode), (1, 2));
    // The session works, and a second connection joins it with its token as usual.
    let (_c2, attached) = Client::attach(srv.addr, &ok).await;
    assert_eq!(attached.server_version, env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn a_signature_cannot_be_replayed_on_another_connection() {
    let srv = server().await;
    let mut first = Client::connect(srv.addr).await;
    let n1 = challenge(&mut first, "alice").await;
    let captured = sign(&n1, "alice"); // what a sniffer would see on the wire
    let mut second = Client::connect(srv.addr).await;
    let n2 = challenge(&mut second, "alice").await;
    assert_ne!(n1, n2, "every connection gets its own nonce");
    second.send_with_seq(1, proof(&captured)).await;
    let Recv::Frame(f) = second.recv(WAIT).await else {
        panic!("no reply")
    };
    expect_error_code(f.body.unwrap(), ErrorCode::Unauthenticated);
    second.expect_closed().await;
    // The original connection, with the real signature, still works.
    first.send_with_seq(1, proof(&captured)).await;
    assert!(matches!(
        first.recv(WAIT).await,
        Recv::Frame(pb::ServerFrame {
            body: Some(sf::Body::HelloOk(_)),
            ..
        })
    ));
}

#[tokio::test]
async fn a_signature_for_another_user_name_is_refused_and_unknown_users_get_a_challenge_too() {
    let srv = server().await;
    let mut c = Client::connect(srv.addr).await;
    let nonce = challenge(&mut c, "alice").await;
    c.send_with_seq(1, proof(&sign(&nonce, "mallory"))).await; // signed for a different name
    let Recv::Frame(f) = c.recv(WAIT).await else {
        panic!("no reply")
    };
    expect_error_code(f.body.unwrap(), ErrorCode::Unauthenticated);
    // A name that does not exist is not told apart from one that does, until the proof is wrong.
    let mut ghost = Client::connect(srv.addr).await;
    let nonce = challenge(&mut ghost, "nobody").await;
    ghost.send_with_seq(1, proof(&sign(&nonce, "nobody"))).await;
    let Recv::Frame(f) = ghost.recv(WAIT).await else {
        panic!("no reply")
    };
    let e = expect_error_code(f.body.unwrap(), ErrorCode::Unauthenticated);
    assert_eq!(e.message, "invalid credentials");
}

#[tokio::test]
async fn unknown_auth_modes_and_out_of_order_messages_are_refused() {
    let srv = server().await;
    for mode in [0, 1, 3, 99] {
        let mut c = Client::connect(srv.addr).await;
        let body = c
            .call(cf::Body::AuthStart(pb::AuthStart {
                protocol_version: 1,
                auth_mode: mode,
                client_id: "alice".into(),
                instance_label: String::new(),
            }))
            .await;
        let e = expect_error_code(body, ErrorCode::UnsupportedVersion);
        assert!(
            e.message.contains("accepts auth modes 1 to 2"),
            "{}",
            e.message
        );
        c.expect_closed().await;
    }
    // AuthProof with no AuthStart before it.
    let mut c = Client::connect(srv.addr).await;
    expect_error_code(c.call(proof(&[0; 64])).await, ErrorCode::InvalidArgument);
    // Anything but AuthProof after AuthStart.
    let mut c = Client::connect(srv.addr).await;
    challenge(&mut c, "alice").await;
    expect_error_code(
        c.call(cf::Body::Ping(pb::Ping { nonce: 1 })).await,
        ErrorCode::InvalidArgument,
    );
    // A signature of the wrong length is just a wrong signature.
    let mut c = Client::connect(srv.addr).await;
    challenge(&mut c, "alice").await;
    c.send_with_seq(1, proof(b"short")).await;
    let Recv::Frame(f) = c.recv(WAIT).await else {
        panic!("no reply")
    };
    expect_error_code(f.body.unwrap(), ErrorCode::Unauthenticated);
}

#[tokio::test]
async fn requiring_keys_turns_password_logins_off() {
    let srv = TestServer::start_with(|c| c.min_auth_mode = 2).await;
    let mut c = Client::connect(srv.addr).await;
    let e = expect_error_code(
        c.call(hello_body("billing", "pw-billing")).await,
        ErrorCode::UnsupportedVersion,
    );
    assert!(
        e.message.contains("password logins are disabled"),
        "{}",
        e.message
    );
}
