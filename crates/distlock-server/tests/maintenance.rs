//! `ResetUsers`: the server re-reads its users file and throws out whoever lost access.
//! Real server, real users file on disk, real TCP.

mod common;

use std::time::Duration;

use common::*;
use distlock_proto::v1 as pb;
use distlock_proto::v1::server_frame as sf;
use pb::{ErrorCode, HolderState, Mode};
use tokio::time::Instant;

fn zero(r: &pb::ResetUsersOk) -> bool {
    (
        r.added,
        r.removed,
        r.disabled,
        r.enabled,
        r.password_changed,
    ) == (0, 0, 0, 0, 0)
        && (r.sessions_closed, r.connections_closed) == (0, 0)
        && !r.invoker_dropped
}

#[tokio::test]
async fn resetting_with_an_unchanged_file_does_nothing() {
    let srv = FileServer::start(&["billing", "reports"]).await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let (mut b, _) = Client::login(srv.addr, "reports").await;
    expect_granted(a.acquire(1, "m", Mode::Mutex, 5_000, 0).await);

    let report = expect_reset(a.reset_users().await);
    assert!(zero(&report), "{report:?}");
    // Nobody was disturbed.
    expect_error_code(
        b.acquire(1, "m", Mode::Mutex, 5_000, 0).await,
        ErrorCode::Busy,
    );
    expect_granted(b.acquire(2, "other", Mode::Mutex, 5_000, 0).await);
}

#[tokio::test]
async fn added_users_can_log_in_afterwards_and_nobody_is_dropped() {
    let srv = FileServer::start(&["billing"]).await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let mut early = Client::connect(srv.addr).await;
    expect_error_code(
        early.call(hello_body("newcomer", "pw-newcomer")).await,
        ErrorCode::Unauthenticated,
    );

    srv.write(&[("billing", true), ("newcomer", true)]);
    let report = expect_reset(a.reset_users().await);
    assert_eq!(
        (report.added, report.removed, report.sessions_closed),
        (1, 0, 0)
    );

    let (mut n, _) = Client::login(srv.addr, "newcomer").await;
    expect_granted(n.acquire(1, "m", Mode::Mutex, 5_000, 0).await);
    expect_granted(a.acquire(1, "mine", Mode::Mutex, 5_000, 0).await); // the old connection still works
}

#[tokio::test]
async fn a_removed_user_is_thrown_out_everywhere_loses_its_locks_and_cannot_come_back() {
    // A real lock-delay, to see that a thrown-out holder is treated like an expired lease.
    let srv = FileServer::start_with(&["billing", "reports"], |c| {
        c.lock.lock_delay = Duration::from_millis(400);
    })
    .await;
    let (mut a1, hello) = Client::login(srv.addr, "billing").await;
    let (mut a2, _) = Client::attach(srv.addr, &hello).await; // a pool: two connections, one session
    let (mut b, _) = Client::login(srv.addr, "reports").await;
    let held = expect_granted(a1.acquire(1, "m", Mode::Mutex, 30_000, 0).await);
    expect_queued(b.acquire(1, "m", Mode::Mutex, 30_000, 30_000).await);

    srv.write(&[("reports", true)]); // billing is gone
    let started = Instant::now();
    let report = expect_reset(b.reset_users().await);
    assert_eq!(
        (
            report.removed,
            report.sessions_closed,
            report.connections_closed,
            report.invoker_dropped
        ),
        (1, 1, 2, false),
        "{report:?}"
    );

    // Both of billing's connections are closed by the server.
    a1.expect_closed().await;
    a2.expect_closed().await;

    // Its lock was released as after a lost lease: tagged ABANDONED, after the lock-delay.
    let grant = expect_granted_push(b.expect_push().await);
    assert!(
        started.elapsed() >= Duration::from_millis(350),
        "handed over inside the lock-delay"
    );
    let previous = grant.previous.expect("tag");
    assert_eq!(previous.state, HolderState::Abandoned as i32);
    assert_eq!(previous.set_by_token, held.fencing_token);

    // It cannot return: its session is gone, and it cannot log in again.
    let mut late = Client::connect(srv.addr).await;
    expect_error_code(
        late.call(attach_body(hello.session_id, &hello.session_token))
            .await,
        ErrorCode::SessionNotFound,
    );
    let mut again = Client::connect(srv.addr).await;
    expect_error_code(
        again.call(hello_body("billing", "pw-billing")).await,
        ErrorCode::Unauthenticated,
    );

    // The user that stayed is unaffected.
    expect_granted(b.acquire(2, "fine", Mode::Mutex, 5_000, 0).await);
}

#[tokio::test]
async fn a_disabled_user_is_thrown_out_too() {
    let srv = FileServer::start(&["billing", "reports"]).await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let (mut b, _) = Client::login(srv.addr, "reports").await;

    srv.write(&[("billing", false), ("reports", true)]);
    let report = expect_reset(b.reset_users().await);
    assert_eq!(
        (report.disabled, report.removed, report.sessions_closed),
        (1, 0, 1)
    );
    a.expect_closed().await;

    let mut again = Client::connect(srv.addr).await;
    expect_error_code(
        again.call(hello_body("billing", "pw-billing")).await,
        ErrorCode::Unauthenticated,
    );

    // Re-enabling lets it back in, with a fresh session.
    srv.write(&[("billing", true), ("reports", true)]);
    let report = expect_reset(b.reset_users().await);
    assert_eq!((report.enabled, report.sessions_closed), (1, 0));
    Client::login(srv.addr, "billing").await;
}

#[tokio::test]
async fn the_sender_can_remove_itself_and_still_learns_what_happened() {
    let srv = FileServer::start(&["billing", "reports"]).await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    expect_granted(a.acquire(1, "m", Mode::Mutex, 30_000, 0).await);

    srv.write(&[("reports", true)]);
    let report = expect_reset(a.reset_users().await); // the reply arrives...
    assert_eq!(
        (
            report.removed,
            report.sessions_closed,
            report.connections_closed,
            report.invoker_dropped
        ),
        (1, 1, 1, true),
        "{report:?}"
    );
    a.expect_closed().await; // ...and then the connection is dropped, as it has to be

    let (mut b, _) = Client::login(srv.addr, "reports").await;
    expect_granted(b.acquire(1, "m", Mode::Mutex, 5_000, 0).await); // its lock is free
}

#[tokio::test]
async fn a_broken_file_changes_nothing_and_the_error_does_not_leak_details() {
    let srv = FileServer::start(&["billing", "reports"]).await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let (mut b, _) = Client::login(srv.addr, "reports").await;
    expect_granted(a.acquire(1, "m", Mode::Mutex, 30_000, 0).await);

    for broken in [
        "{ not json".to_owned(),
        r#"{"version": 1, "users": []}"#.to_owned(),
        r#"{"version": 1, "users": [{"client_id": "billing-leaky", "password_hash": "nope"}]}"#
            .to_owned(),
    ] {
        srv.write_raw(&broken);
        let e = expect_error_code(a.reset_users().await, ErrorCode::MaintenanceFailed);
        assert!(
            !e.message.contains("billing")
                && !e.message.contains("users.json")
                && !e.message.contains('/'),
            "the reply must not reveal user names or paths: {}",
            e.message
        );
    }

    // Everything still works, for everyone, including new logins.
    expect_error_code(
        b.acquire(1, "m", Mode::Mutex, 5_000, 0).await,
        ErrorCode::Busy,
    );
    Client::login(srv.addr, "reports").await;
    Client::login(srv.addr, "billing").await;

    // Repairing the file and resetting again works.
    srv.write(&[("billing", true), ("reports", true), ("newcomer", true)]);
    assert_eq!(expect_reset(a.reset_users().await).added, 1);
}

#[tokio::test]
async fn a_changed_password_applies_to_new_logins_and_leaves_live_sessions_alone() {
    let srv = FileServer::start(&["billing", "reports"]).await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    expect_granted(a.acquire(1, "m", Mode::Mutex, 30_000, 0).await);

    srv.write_with_passwords(&[
        ("billing", "rotated", true),
        ("reports", "pw-reports", true),
    ]);
    let report = expect_reset(a.reset_users().await);
    assert_eq!((report.password_changed, report.sessions_closed), (1, 0));

    // The live session carries on...
    expect_granted(a.acquire(2, "n", Mode::Mutex, 5_000, 0).await);
    // ...but new logins need the new password.
    let mut old = Client::connect(srv.addr).await;
    expect_error_code(
        old.call(hello_body("billing", "pw-billing")).await,
        ErrorCode::Unauthenticated,
    );
    let mut new = Client::connect(srv.addr).await;
    assert!(matches!(
        new.call(hello_body("billing", "rotated")).await,
        sf::Body::HelloOk(_)
    ));
}

#[tokio::test]
async fn users_built_in_memory_cannot_be_reset_and_say_so_without_dropping_anyone() {
    let srv = TestServer::start().await; // in-memory users: no file behind them
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    expect_error_code(a.reset_users().await, ErrorCode::MaintenanceFailed);
    expect_granted(a.acquire(1, "m", Mode::Mutex, 5_000, 0).await);
}

#[tokio::test]
async fn an_empty_maintenance_command_is_rejected_politely() {
    let srv = TestServer::start().await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let body = a
        .call(distlock_proto::v1::client_frame::Body::Maintenance(
            pb::Maintenance { command: None },
        ))
        .await;
    expect_error_code(body, ErrorCode::InvalidArgument);
    expect_granted(a.acquire(1, "m", Mode::Mutex, 5_000, 0).await); // connection still fine
}

#[tokio::test]
async fn the_reply_carries_the_md5_of_the_file_in_force() {
    let srv = FileServer::start(&["billing"]).await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let first = srv.md5();
    assert_eq!(first.len(), 32);
    // An unchanged file: nothing changes, and the hash is the one the server started with.
    assert_eq!(expect_reset(a.reset_users().await).users_md5, first);
    // The file changes on disk; the hash only follows once the server has read it.
    srv.write(&[("billing", true), ("newcomer", true)]);
    let second = srv.md5();
    assert_ne!(first, second);
    assert_eq!(expect_reset(a.reset_users().await).users_md5, second);
    // A file that cannot be loaded changes nothing, and the next reply still says what is in force.
    std::fs::write(srv.path(), "{ not json").unwrap();
    assert!(matches!(a.reset_users().await, sf::Body::Error(_)));
    std::fs::write(
        srv.path(),
        serde_json::to_vec(&serde_json::json!({"version": 1, "users": []})).unwrap(),
    )
    .unwrap();
    assert!(
        matches!(a.reset_users().await, sf::Body::Error(_)),
        "an empty list is invalid too"
    );
    srv.write(&[("billing", true), ("newcomer", true)]);
    assert_eq!(expect_reset(a.reset_users().await).users_md5, second);
}
