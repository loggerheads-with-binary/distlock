//! Login by Ed25519 key (auth mode 2), the server's minimum auth mode, and the `min_version` check.
mod common;
use common::*;
use distlock_client::{Acquire, DistLockClient, Error};
use distlock_server::UserDb;
use ring::signature::{Ed25519KeyPair, KeyPair};

fn pair(n: u8) -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(&[n; 32]).unwrap()
}
fn public(n: u8) -> [u8; 32] {
    pair(n).public_key().as_ref().try_into().unwrap()
}
/// A client that logs in as `user` with the private key built from `seed_byte`.
fn key_client(srv: &TestServer, user: &str, seed_byte: u8) -> DistLockClient {
    DistLockClient {
        auth_mode: 2,
        password: distlock_proto::encode_key(&[seed_byte; 32]),
        pool_size: 2,
        ..srv.client(user)
    }
}
async fn server() -> TestServer {
    // alice and bob log in by key only; carol has a password and no key.
    TestServer::start_with_db(
        "127.0.0.1:0".parse().unwrap(),
        UserDb::parse(&users_json(), 1).unwrap(),
        |_| {},
    )
    .await
}
fn users_json() -> String {
    use distlock_proto::encode_key as b64;
    serde_json::json!({ "version": 1, "users": [
        { "client_id": "alice", "public_keys": [{ "key": b64(&public(1)), "identifier": "laptop" }] },
        { "client_id": "bob", "public_keys": [{ "key": b64(&public(2)) }] },
        { "client_id": "old", "public_keys": [{ "key": b64(&public(3)), "expiry": "2001-01-01T00:00:00Z" }] },
        { "client_id": "carol", "password_hash": bcrypt::hash("pw-carol", 10).unwrap() },
    ]})
    .to_string()
}

#[tokio::test]
async fn a_key_logs_in_and_the_whole_pool_works() {
    let srv = server().await;
    let c = key_client(&srv, "alice", 1).connect().await.unwrap();
    // The second pool connection joins with the session token, not a second signature.
    let lock = c.acquire(Acquire::mutex("m")).await.unwrap();
    lock.release().await.unwrap();
    assert_eq!(c.stats().await.unwrap().connected_clients, 1);
}

#[tokio::test]
async fn the_wrong_key_an_unknown_user_and_an_expired_key_all_get_the_same_answer() {
    let srv = server().await;
    let mut messages = Vec::new();
    for (user, seed) in [
        ("alice", 2),
        ("alice", 9),
        ("nobody", 1),
        ("old", 3),
        ("carol", 1),
    ] {
        match key_client(&srv, user, seed).connect().await {
            Err(Error::Auth(m)) => messages.push(m),
            other => panic!(
                "{user}/{seed}: expected an auth failure, got {:?}",
                other.map(|_| ())
            ),
        }
    }
    assert!(messages.iter().all(|m| m == &messages[0]), "{messages:?}");
}

#[tokio::test]
async fn passwords_and_keys_do_not_cross_over() {
    let srv = server().await;
    // A password login for a key-only user fails, whatever the password is.
    let by_password = DistLockClient {
        password: "anything".into(),
        ..srv.client("alice")
    };
    assert!(matches!(by_password.connect().await, Err(Error::Auth(_))));
    // A user with a password but no key cannot use a key. And carol's password still works.
    assert!(matches!(
        key_client(&srv, "carol", 1).connect().await,
        Err(Error::Auth(_))
    ));
    let carol = DistLockClient {
        password: "pw-carol".into(),
        ..srv.client("carol")
    };
    assert!(carol.connect().await.is_ok());
}

#[tokio::test]
async fn a_server_can_require_keys() {
    let srv = TestServer::start_with_db(
        "127.0.0.1:0".parse().unwrap(),
        UserDb::parse(&users_json(), 1).unwrap(),
        |c| c.min_auth_mode = 2,
    )
    .await;
    let carol = DistLockClient {
        password: "pw-carol".into(),
        ..srv.client("carol")
    };
    match carol.connect().await {
        Err(Error::Server { code, message, .. }) => {
            assert_eq!(code, distlock_proto::v1::ErrorCode::UnsupportedVersion);
            assert!(
                message.contains("password logins are disabled"),
                "{message}"
            );
        }
        other => panic!(
            "expected the server to refuse passwords: {:?}",
            other.map(|_| ())
        ),
    }
    assert!(key_client(&srv, "alice", 1).connect().await.is_ok());
}

#[tokio::test]
async fn bad_auth_settings_are_refused_before_connecting() {
    let srv = server().await;
    for (c, what) in [
        (
            DistLockClient {
                password: "not a key".into(),
                ..key_client(&srv, "alice", 1)
            },
            "private key",
        ),
        (
            DistLockClient {
                password: distlock_proto::encode_key(&[1; 16]),
                ..key_client(&srv, "alice", 1)
            },
            "private key",
        ),
        (
            DistLockClient {
                auth_mode: 3,
                ..key_client(&srv, "alice", 1)
            },
            "auth_mode",
        ),
        (
            DistLockClient {
                auth_mode: 0,
                ..key_client(&srv, "alice", 1)
            },
            "auth_mode",
        ),
        (
            DistLockClient {
                min_version: Some("soon".into()),
                ..key_client(&srv, "alice", 1)
            },
            "min_version",
        ),
    ] {
        match c.connect().await {
            Err(Error::Config(m)) => assert!(m.contains(what), "{what}: {m}"),
            other => panic!(
                "{what}: expected a Config error, got {:?}",
                other.map(|_| ())
            ),
        }
    }
}

#[tokio::test]
async fn min_version_is_checked_after_logging_in() {
    let srv = server().await;
    let with = |min: Option<&str>| DistLockClient {
        min_version: min.map(str::to_owned),
        ..key_client(&srv, "alice", 1)
    };
    assert!(
        with(None).connect().await.is_ok(),
        "the default is what this client was built for"
    );
    assert!(with(Some("0.0.0")).connect().await.is_ok());
    assert!(
        with(Some(env!("CARGO_PKG_VERSION")))
            .connect()
            .await
            .is_ok(),
        "the server's own version"
    );
    match with(Some("99.0.0")).connect().await {
        Err(Error::ServerTooOld { server, required }) => {
            assert_eq!(
                (server.as_str(), required.as_str()),
                (env!("CARGO_PKG_VERSION"), "99.0.0")
            );
        }
        other => panic!("expected ServerTooOld, got {:?}", other.map(|_| ())),
    }
    // It applies to password logins too.
    let by_password = DistLockClient {
        password: "pw-carol".into(),
        min_version: Some("99.0.0".into()),
        ..srv.client("carol")
    };
    assert!(matches!(
        by_password.connect().await,
        Err(Error::ServerTooOld { .. })
    ));
}
