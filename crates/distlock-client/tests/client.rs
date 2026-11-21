//! The async client against the real server. Timing assertions are lower bounds only.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::*;
use distlock_client::{Acquire, DistLockClient, Error, Kind, State};
use tokio::time::Instant;

// ------------------------------------------------------------------------------------ connecting

#[tokio::test]
async fn connecting_creates_a_session() {
    let srv = TestServer::start().await;
    let client = srv.client("billing").connect().await.unwrap();
    assert!(client.session_id().is_some());
    let other = srv.client("billing").connect().await.unwrap();
    assert_ne!(
        client.session_id(),
        other.session_id(),
        "each client is its own session"
    );
}

#[tokio::test]
async fn wrong_credentials_fail_the_connect() {
    let srv = TestServer::start().await;
    let bad = DistLockClient {
        password: "nope".into(),
        ..srv.client("billing")
    };
    assert!(matches!(bad.connect().await, Err(Error::Auth(_))));
    let unknown = DistLockClient {
        client_id: "ghost".into(),
        ..srv.client("billing")
    };
    assert!(matches!(unknown.connect().await, Err(Error::Auth(_))));
}

#[tokio::test]
async fn bad_settings_are_rejected_before_connecting() {
    let srv = TestServer::start().await;
    let base = srv.client("billing");
    let cases: Vec<(DistLockClient, &str)> = vec![
        (
            DistLockClient {
                client_id: String::new(),
                ..base.clone()
            },
            "client_id",
        ),
        (
            DistLockClient {
                host: String::new(),
                ..base.clone()
            },
            "host",
        ),
        (
            DistLockClient {
                port: 0,
                ..base.clone()
            },
            "port",
        ),
        (
            DistLockClient {
                pool_size: 0,
                ..base.clone()
            },
            "pool_size",
        ),
        (
            DistLockClient {
                pool_size: 1000,
                ..base.clone()
            },
            "pool_size",
        ),
        (
            DistLockClient {
                default_ttl: Duration::ZERO,
                ..base.clone()
            },
            "default_ttl",
        ),
        (
            DistLockClient {
                namespace: Some("a:b".into()),
                ..base.clone()
            },
            "namespace",
        ),
    ];
    for (settings, what) in cases {
        match settings.connect().await {
            Err(Error::Config(m)) => assert!(m.contains(what), "{what}: {m}"),
            other => panic!("{what}: expected a Config error, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn an_unreachable_server_fails_the_connect() {
    // Bind and drop to get a port nobody listens on.
    let port = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let settings = DistLockClient {
        client_id: "billing".into(),
        password: "x".into(),
        port,
        ..Default::default()
    };
    assert!(matches!(
        settings.connect().await,
        Err(Error::Connect { .. })
    ));
}

#[test]
fn the_password_never_appears_in_debug_output() {
    let settings = DistLockClient {
        client_id: "a".into(),
        password: "hunter2".into(),
        ..Default::default()
    };
    assert!(!format!("{settings:?}").contains("hunter2"));
}

// ------------------------------------------------------------------------------------ locking

#[tokio::test]
async fn a_mutex_excludes_and_try_acquire_reports_none() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();

    let lock = a.acquire(Acquire::mutex("m")).await.unwrap();
    assert!(b.try_acquire(Acquire::mutex("m")).await.unwrap().is_none());
    assert!(matches!(
        b.acquire(Acquire::mutex("m").wait(Duration::ZERO)).await,
        Err(Error::Busy)
    ));
    lock.release().await.unwrap();
    assert!(b.try_acquire(Acquire::mutex("m")).await.unwrap().is_some());
}

#[tokio::test]
async fn a_waiting_acquire_gets_the_lock_when_it_is_released_and_a_timeout_is_reported() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let lock = a.acquire(Acquire::mutex("m")).await.unwrap();

    let t0 = Instant::now();
    assert!(matches!(
        b.acquire(Acquire::mutex("m").wait(ms(200))).await,
        Err(Error::Timeout)
    ));
    assert!(t0.elapsed() >= ms(200));

    let waiter = {
        let b = b.clone();
        tokio::spawn(async move { b.acquire(Acquire::mutex("m")).await })
    };
    tokio::time::sleep(ms(150)).await;
    assert!(!waiter.is_finished(), "must still be waiting");
    lock.release().await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(got.fencing_token() > 0);
}

#[tokio::test]
async fn namespaces_keep_applications_apart_and_can_be_overridden() {
    let srv = TestServer::start().await;
    let a = DistLockClient {
        namespace: Some("app-a".into()),
        ..srv.client("billing")
    }
    .connect()
    .await
    .unwrap();
    let b = DistLockClient {
        namespace: Some("app-b".into()),
        ..srv.client("reports")
    }
    .connect()
    .await
    .unwrap();

    let _held = a.acquire(Acquire::mutex("job")).await.unwrap();
    assert!(
        b.try_acquire(Acquire::mutex("job"))
            .await
            .unwrap()
            .is_some(),
        "other namespace: no conflict"
    );
    // Overriding the namespace on one acquire puts it in the other application's space.
    assert!(
        b.try_acquire(Acquire::mutex("job").namespace("app-a"))
            .await
            .unwrap()
            .is_none()
    );
    // An empty override means no namespace at all.
    assert!(
        b.try_acquire(Acquire::mutex("job").namespace(""))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn mutexes_semaphores_and_read_write_locks_are_separate_families_and_read_write_conflict() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();

    // Same name, different family: no conflict, no kind mismatch.
    let _m = a.acquire(Acquire::mutex("x")).await.unwrap();
    let _w = b
        .try_acquire(Acquire::write("x"))
        .await
        .unwrap()
        .expect("a write lock named like a held mutex");
    let _s = b
        .try_acquire(Acquire::semaphore("x", 1, 1))
        .await
        .unwrap()
        .expect("a semaphore with the same name");

    // A write lock blocks readers and writers of the same name; readers share and block writers.
    assert!(a.try_acquire(Acquire::read("x")).await.unwrap().is_none());
    assert!(a.try_acquire(Acquire::write("x")).await.unwrap().is_none());
    _w.release().await.unwrap();
    let r1 = a
        .try_acquire(Acquire::read("x"))
        .await
        .unwrap()
        .expect("first reader");
    let r2 = b
        .try_acquire(Acquire::read("x"))
        .await
        .unwrap()
        .expect("readers share");
    assert!(a.try_acquire(Acquire::write("x")).await.unwrap().is_none());
    r1.release().await.unwrap();
    r2.release().await.unwrap();
    assert!(a.try_acquire(Acquire::write("x")).await.unwrap().is_some());
}

#[tokio::test]
async fn the_previous_holders_status_reaches_the_next_holder() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();

    let first = a.acquire(Acquire::mutex("job")).await.unwrap();
    assert_eq!(first.previous(), None);
    let first_token = first.fencing_token();
    first
        .release_with(Some(State::Succeeded), None)
        .await
        .unwrap();

    let second = b.acquire(Acquire::mutex("job")).await.unwrap();
    let prev = second.previous().expect("status");
    assert_eq!(
        (prev.state, prev.set_by_token),
        (State::Succeeded, first_token)
    );
    second.release().await.unwrap(); // no status: leaves the old one

    let third = a.acquire(Acquire::mutex("job")).await.unwrap();
    assert_eq!(third.previous().map(|p| p.state), Some(State::Succeeded));
    third.release_with(Some(State::Failed), None).await.unwrap();
    let fourth = b.acquire(Acquire::mutex("job")).await.unwrap();
    assert_eq!(fourth.previous().map(|p| p.state), Some(State::Failed));
}

#[tokio::test]
async fn only_exclusive_locks_can_set_a_status_and_abandoned_is_not_settable() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();

    let reader = a.acquire(Acquire::read("r")).await.unwrap();
    assert!(matches!(
        reader.release_with(Some(State::Succeeded), None).await,
        Err(Error::InvalidArgument(_))
    ));
    // The rejected release still gave the lock back (the lock is consumed either way).
    assert!(a.try_acquire(Acquire::write("r")).await.unwrap().is_some());

    let m = a.acquire(Acquire::mutex("m")).await.unwrap();
    assert!(matches!(
        m.release_with(Some(State::Abandoned), None).await,
        Err(Error::InvalidArgument(_))
    ));
    assert!(a.try_acquire(Acquire::mutex("m")).await.unwrap().is_some());
}

#[tokio::test]
async fn semaphores_limit_concurrency_and_report_capacity_mismatches() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();

    let s1 = a.acquire(Acquire::semaphore("pool", 2, 1)).await.unwrap();
    let _s2 = b.acquire(Acquire::semaphore("pool", 2, 1)).await.unwrap();
    assert!(
        a.try_acquire(Acquire::semaphore("pool", 2, 1))
            .await
            .unwrap()
            .is_none()
    );
    match a.try_acquire(Acquire::semaphore("pool", 3, 1)).await {
        Err(Error::Server {
            existing_capacity, ..
        }) => assert_eq!(existing_capacity, 2),
        other => panic!("expected a capacity mismatch, got {other:?}"),
    }
    s1.release().await.unwrap();
    assert!(
        a.try_acquire(Acquire::semaphore("pool", 2, 1))
            .await
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        a.acquire(Acquire::new(
            "bad",
            Kind::Semaphore {
                capacity: 1,
                permits: 2
            }
        ))
        .await,
        Err(Error::InvalidArgument(_))
    ));
}

#[tokio::test]
async fn many_tasks_contending_for_one_lock_are_mutually_exclusive() {
    let srv = TestServer::start().await;
    let client = DistLockClient {
        pool_size: 4,
        ..srv.client("billing")
    }
    .connect()
    .await
    .unwrap();
    let counter = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        let counter = Arc::clone(&counter);
        tasks.push(tokio::spawn(async move {
            for _ in 0..8 {
                let lock = client.acquire(Acquire::mutex("counter")).await.unwrap();
                // A read-modify-write that loses updates unless the lock really excludes.
                let seen = counter.load(Ordering::SeqCst);
                tokio::task::yield_now().await;
                counter.store(seen + 1, Ordering::SeqCst);
                lock.release().await.unwrap();
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(counter.load(Ordering::SeqCst), 64);
}

// ------------------------------------------------------------------------------------ leases

#[tokio::test]
async fn the_lease_is_renewed_for_as_long_as_the_lock_is_held() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();

    let lock = a.acquire(Acquire::mutex("m").ttl(ms(300))).await.unwrap();
    tokio::time::sleep(ms(1_200)).await; // four times the lease
    assert!(!lock.is_lost());
    assert!(
        b.try_acquire(Acquire::mutex("m")).await.unwrap().is_none(),
        "still held"
    );
    lock.release().await.unwrap();
}

#[tokio::test]
async fn a_lease_the_server_ends_is_reported_as_lost() {
    // max_hold caps a lease's total life however often it is renewed.
    let srv = TestServer::start_with("127.0.0.1:0".parse().unwrap(), |c| {
        c.lock.max_hold = Some(ms(500))
    })
    .await;
    let a = srv.client("billing").connect().await.unwrap();
    let t0 = Instant::now();
    let lock = a.acquire(Acquire::mutex("m").ttl(ms(300))).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), lock.wait_lost())
        .await
        .expect("lost within 5s");
    assert!(
        t0.elapsed() >= ms(450),
        "lost too early: {:?}",
        t0.elapsed()
    );
    assert!(lock.is_lost());
    assert!(matches!(lock.release().await, Err(Error::LeaseLost)));
}

#[tokio::test]
async fn dropping_a_lock_releases_it() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let lock = a.acquire(Acquire::mutex("m")).await.unwrap();
    drop(lock);
    let got = tokio::time::timeout(Duration::from_secs(3), b.acquire(Acquire::mutex("m"))).await;
    assert!(got.expect("freed promptly").is_ok());
}

#[tokio::test]
async fn cancelling_an_acquire_does_not_leave_a_phantom_waiter() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let c = DistLockClient {
        client_id: "billing".into(),
        ..srv.client("billing")
    }
    .connect()
    .await
    .unwrap();

    let held = a.acquire(Acquire::mutex("m")).await.unwrap();
    let abandoned = {
        let b = b.clone();
        tokio::spawn(async move { b.acquire(Acquire::mutex("m")).await })
    };
    tokio::time::sleep(ms(200)).await; // queued
    abandoned.abort();
    let _ = abandoned.await;
    tokio::time::sleep(ms(200)).await; // let the cancel reach the server

    held.release().await.unwrap();
    // Had the aborted waiter stayed queued, the lock would have been granted to it and stuck.
    let got =
        tokio::time::timeout(Duration::from_secs(3), c.try_acquire(Acquire::mutex("m"))).await;
    assert!(got.expect("answered").unwrap().is_some());
}

#[tokio::test]
async fn close_releases_everything_at_once() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let _l1 = a.acquire(Acquire::mutex("one")).await.unwrap();
    let _l2 = a.acquire(Acquire::write("two")).await.unwrap();
    a.close().await.unwrap();
    assert!(
        b.try_acquire(Acquire::mutex("one"))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        b.try_acquire(Acquire::write("two"))
            .await
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        a.acquire(Acquire::mutex("three")).await,
        Err(Error::Closed)
    ));
}

#[tokio::test]
async fn acquires_wait_out_the_servers_startup_grace_period() {
    let srv = TestServer::start_with("127.0.0.1:0".parse().unwrap(), |c| {
        c.lock.startup_grace = ms(700)
    })
    .await;
    let a = srv.client("billing").connect().await.unwrap();
    let t0 = Instant::now();
    let lock = a
        .acquire(Acquire::mutex("m"))
        .await
        .expect("granted once the server is ready");
    assert!(
        t0.elapsed() >= ms(500),
        "granted during the grace period: {:?}",
        t0.elapsed()
    );
    lock.release().await.unwrap();
}

// ------------------------------------------------------------------------------------ reconnect

#[tokio::test]
async fn locks_survive_dropped_connections_and_are_reclaimed() {
    let srv = TestServer::start().await;
    let proxy = Proxy::start(srv.addr).await;
    let other = srv.client("reports").connect().await.unwrap();
    let c = client_for(proxy.addr, "billing").connect().await.unwrap();
    let session = c.session_id();

    let lock = c.acquire(Acquire::mutex("m")).await.unwrap();
    proxy.cut();
    tokio::time::sleep(ms(1_500)).await; // reconnect, Attach, reconcile
    assert!(
        !lock.is_lost(),
        "a dropped connection must not lose the lock"
    );
    assert_eq!(c.session_id(), session, "the same session was rejoined");
    assert!(
        other
            .try_acquire(Acquire::mutex("m"))
            .await
            .unwrap()
            .is_none(),
        "still held"
    );

    // The rejoined pool works for new requests and for releasing the old lock.
    let second = c.acquire(Acquire::mutex("n")).await.unwrap();
    second.release().await.unwrap();
    lock.release().await.unwrap();
    assert!(
        other
            .try_acquire(Acquire::mutex("m"))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_grant_made_while_disconnected_is_picked_up_on_reconnect() {
    let srv = TestServer::start().await;
    let proxy = Proxy::start(srv.addr).await;
    let holder = srv.client("reports").connect().await.unwrap();
    let c = client_for(proxy.addr, "billing").connect().await.unwrap();

    let held = holder.acquire(Acquire::mutex("m")).await.unwrap();
    let waiter = {
        let c = c.clone();
        tokio::spawn(async move {
            c.acquire(Acquire::mutex("m").wait(Duration::from_secs(30)))
                .await
        })
    };
    tokio::time::sleep(ms(300)).await; // queued on the server

    proxy.pause();
    proxy.cut();
    tokio::time::sleep(ms(400)).await; // the client is now fully disconnected
    held.release_with(Some(State::Succeeded), None)
        .await
        .unwrap(); // granted to a session with no connection
    tokio::time::sleep(ms(200)).await;
    proxy.resume();

    let lock = tokio::time::timeout(Duration::from_secs(15), waiter)
        .await
        .expect("resolved after reconnecting")
        .unwrap()
        .expect("the grant must be recovered, not lost");
    assert_eq!(
        lock.previous().map(|p| p.state),
        Some(State::Succeeded),
        "the previous holder's status must survive the lost push"
    );
    assert!(!lock.is_lost());
}

#[tokio::test]
async fn a_lease_that_expired_while_disconnected_is_noticed_on_reconnect() {
    let srv = TestServer::start().await;
    let proxy = Proxy::start(srv.addr).await;
    let c = client_for(proxy.addr, "billing").connect().await.unwrap();
    let lock = c.acquire(Acquire::mutex("m").ttl(ms(500))).await.unwrap();

    proxy.pause();
    proxy.cut();
    tokio::time::sleep(ms(1_200)).await; // the lease runs out server-side
    proxy.resume();
    tokio::time::timeout(Duration::from_secs(15), lock.wait_lost())
        .await
        .expect("noticed");
    assert!(lock.is_lost());
}

#[tokio::test]
async fn a_server_restart_loses_the_session_and_the_client_logs_in_again() {
    // A fixed port so the replacement server can take over the old address.
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let first = TestServer::start_with(addr, |_| {}).await;
    let c = first.client("billing").connect().await.unwrap();
    let old_session = c.session_id().unwrap();
    let lock = c.acquire(Acquire::mutex("m")).await.unwrap();

    first.stop().await;
    let second = TestServer::start_with(addr, |_| {}).await;

    tokio::time::timeout(Duration::from_secs(15), lock.wait_lost())
        .await
        .expect("the old lock is reported lost once the new server does not know the session");
    // The client logged in again by itself and can lock on the new server.
    let fresh = tokio::time::timeout(Duration::from_secs(15), c.acquire(Acquire::mutex("m")))
        .await
        .expect("reconnected")
        .expect("granted by the new server");
    assert_ne!(c.session_id().unwrap(), old_session);
    fresh.release().await.unwrap();
    drop(second);
}
