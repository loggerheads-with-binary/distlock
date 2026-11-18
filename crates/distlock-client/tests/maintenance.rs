//! `client.maintenance().reset_users()`: reload the server's users, and what that does to clients
//! that lose access (including the one that asked). Real server, real users file.

mod common;

use std::time::Duration;

use common::*;
use distlock_client::{Acquire, DistLockClient, Error};

fn pooled(srv: &FileServer, user: &str, pool_size: usize) -> DistLockClient {
    DistLockClient {
        pool_size,
        ..srv.client(user)
    }
}

#[tokio::test]
async fn resetting_reports_changes_and_leaves_the_remaining_users_undisturbed() {
    let srv = FileServer::start(&["billing", "reports"]).await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let held = a.acquire(Acquire::mutex("m")).await.unwrap();

    let noop = b.maintenance().reset_users().await.unwrap();
    assert!(noop.is_noop(), "{noop:?}");

    srv.write(&[("billing", true), ("reports", true), ("newcomer", true)]);
    let report = b.maintenance().reset_users().await.unwrap();
    assert_eq!(
        (report.added, report.removed, report.sessions_closed),
        (1, 0, 0)
    );
    assert!(
        srv.client("newcomer").connect().await.is_ok(),
        "the new user can log in"
    );

    assert!(!held.is_lost(), "nobody was thrown out");
    assert!(b.try_acquire(Acquire::mutex("m")).await.unwrap().is_none());
    held.release().await.unwrap();
}

#[tokio::test]
async fn a_removed_clients_whole_pool_is_dropped_its_locks_are_lost_and_it_is_told_why() {
    let srv = FileServer::start(&["billing", "reports"]).await;
    let a = pooled(&srv, "billing", 2).connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let lock = a.acquire(Acquire::mutex("m")).await.unwrap();
    assert!(b.try_acquire(Acquire::mutex("m")).await.unwrap().is_none());

    srv.write(&[("reports", true)]);
    let report = b.maintenance().reset_users().await.unwrap();
    assert_eq!(
        (
            report.removed,
            report.sessions_closed,
            report.invoker_dropped
        ),
        (1, 1, false),
        "{report:?}"
    );
    assert_eq!(
        report.connections_closed, 2,
        "both pool connections were closed by the server"
    );

    // Its lock is gone from the server's side at once...
    let freed = b.try_acquire(Acquire::mutex("m")).await.unwrap();
    assert!(
        freed.is_some(),
        "the removed client's lock must be released"
    );
    // ...and the client itself learns it quickly, and then cannot get back in.
    tokio::time::timeout(Duration::from_secs(5), lock.wait_lost())
        .await
        .expect("the removed client noticed");
    let err = a.acquire(Acquire::mutex("x")).await.unwrap_err();
    assert!(
        matches!(err, Error::Auth(_)),
        "expected an authentication error, got {err:?}"
    );
    assert_eq!(a.connections(), 0);
}

#[tokio::test]
async fn a_client_that_removes_itself_still_gets_its_report_and_is_then_locked_out() {
    let srv = FileServer::start(&["billing", "reports"]).await;
    let a = pooled(&srv, "billing", 3).connect().await.unwrap();
    let lock = a.acquire(Acquire::mutex("m")).await.unwrap();

    srv.write(&[("reports", true)]);
    let report = a
        .maintenance()
        .reset_users()
        .await
        .expect("the reply must arrive before the drop");
    assert_eq!(
        (
            report.removed,
            report.sessions_closed,
            report.invoker_dropped
        ),
        (1, 1, true)
    );
    assert!(report.connections_closed >= 1);

    tokio::time::timeout(Duration::from_secs(5), lock.wait_lost())
        .await
        .expect("lost");
    assert!(matches!(
        a.acquire(Acquire::mutex("x")).await,
        Err(Error::Auth(_))
    ));
}

#[tokio::test]
async fn a_user_that_is_added_back_recovers_without_the_application_doing_anything() {
    let srv = FileServer::start(&["billing", "reports"]).await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();

    srv.write(&[("reports", true)]);
    b.maintenance().reset_users().await.unwrap();
    assert!(matches!(
        a.acquire(Acquire::mutex("x")).await,
        Err(Error::Auth(_))
    ));

    srv.write(&[("billing", true), ("reports", true)]);
    let report = b.maintenance().reset_users().await.unwrap();
    assert_eq!(report.added, 1);
    // The client was retrying in the background (backoff of up to 5 s) and logs in again by itself.
    let lock = tokio::time::timeout(Duration::from_secs(20), a.acquire(Acquire::mutex("x")))
        .await
        .expect("recovered")
        .expect("granted on a fresh session");
    lock.release().await.unwrap();
}

#[tokio::test]
async fn an_unloadable_file_is_reported_as_a_maintenance_error_and_changes_nothing() {
    let srv = FileServer::start(&["billing", "reports"]).await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let lock = a.acquire(Acquire::mutex("m")).await.unwrap();

    srv.write_raw("{ definitely not json");
    let err = b.maintenance().reset_users().await.unwrap_err();
    match &err {
        Error::Maintenance(message) => assert!(!message.contains("billing"), "{message}"),
        other => panic!("expected a maintenance error, got {other:?}"),
    }
    assert_eq!(err.kind(), "maintenance");
    assert!(!lock.is_lost());
    assert!(b.try_acquire(Acquire::mutex("m")).await.unwrap().is_none());
    assert!(
        srv.client("billing").connect().await.is_ok(),
        "logins still work"
    );
}

#[test]
fn the_blocking_client_has_the_same_command() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let srv = rt.block_on(FileServer::start(&["billing", "reports"]));
    let a = srv.client("billing").connect_blocking().unwrap();
    let b = srv.client("reports").connect_blocking().unwrap();
    assert!(b.maintenance().reset_users().unwrap().is_noop());

    srv.write(&[("reports", true)]);
    let report = b.maintenance().reset_users().unwrap();
    assert_eq!((report.removed, report.sessions_closed), (1, 1));
    assert!(matches!(
        a.acquire(Acquire::mutex("x")),
        Err(Error::Auth(_))
    ));
    drop((a, b));
    drop(srv);
    drop(rt);
}
