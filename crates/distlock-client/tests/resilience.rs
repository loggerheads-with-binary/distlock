//! Pool, reconnect, heartbeat and lease behaviour on bad networks. A TCP proxy in front of the real
//! server simulates cut connections, partitions (a black hole: nothing arrives, nothing errors),
//! refused connections and flapping.

mod common;

use std::time::Duration;

use common::*;
use distlock_client::{Acquire, DistLockClient, Error};
use tokio::time::Instant;

async fn wait_for(what: &str, within: Duration, mut condition: impl FnMut() -> bool) -> Duration {
    let started = Instant::now();
    while !condition() {
        assert!(started.elapsed() < within, "gave up waiting for: {what}");
        tokio::time::sleep(ms(25)).await;
    }
    started.elapsed()
}

fn any_port() -> std::net::SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

async fn proxied_client(
    proxy: &Proxy,
    pool_size: usize,
    ttl: Option<Duration>,
) -> distlock_client::Client {
    let settings = DistLockClient {
        pool_size,
        default_ttl: ttl.unwrap_or(Duration::from_secs(30)),
        ..client_for(proxy.addr, "billing")
    };
    let client = settings.connect().await.unwrap();
    wait_for("the pool to come up", Duration::from_secs(5), || {
        client.connections() == pool_size
    })
    .await;
    client
}

// ------------------------------------------------------------------------------ heartbeat

#[tokio::test]
async fn heartbeats_keep_idle_connections_open_far_past_the_servers_idle_timeout() {
    let srv = TestServer::start_with(any_port(), |c| {
        c.ping_interval = ms(200);
        c.idle_connection_timeout = ms(600);
    })
    .await;
    let proxy = Proxy::start(srv.addr).await;
    let client = proxied_client(&proxy, 3, None).await;

    let accepted = proxy.accepted();
    tokio::time::sleep(ms(3_000)).await; // five idle timeouts with no requests at all
    assert_eq!(client.connections(), 3);
    assert_eq!(
        client.reconnects(),
        0,
        "an idle but heartbeating connection must stay up"
    );
    assert_eq!(proxy.accepted(), accepted);
    assert!(client.ping().await.unwrap() < ms(500));
}

#[tokio::test]
async fn a_black_holed_connection_is_detected_within_one_ping_interval_and_replaced() {
    // The server asks for a ping every 2 s. Connections that go silent must be dropped after about
    // that long, not after several multiples of it.
    let srv = TestServer::start_with(any_port(), |c| {
        c.ping_interval = Duration::from_secs(2);
        c.idle_connection_timeout = Duration::from_secs(60);
    })
    .await;
    let proxy = Proxy::start(srv.addr).await;
    let client = proxied_client(&proxy, 3, None).await;
    let lock = client.acquire(Acquire::mutex("m")).await.unwrap();

    proxy.blackhole();
    let took = wait_for(
        "the pool to replace its dead connections",
        Duration::from_secs(20),
        || client.reconnects() >= 3 && client.connections() == 3,
    )
    .await;
    println!("partition noticed and pool restored after {took:?}");
    assert!(
        took < Duration::from_secs(4),
        "took {took:?} to notice a partition"
    );
    assert!(
        !lock.is_lost(),
        "a partition shorter than the lease must not lose the lock"
    );
    client.ping().await.unwrap();
    lock.release().await.unwrap();
}

#[tokio::test]
async fn a_short_lease_survives_a_black_holed_connection() {
    let srv = TestServer::start_with(any_port(), |c| {
        c.ping_interval = ms(300);
        c.idle_connection_timeout = Duration::from_secs(60);
    })
    .await;
    let other = srv.client("reports").connect().await.unwrap();
    let proxy = Proxy::start(srv.addr).await;
    let client = proxied_client(&proxy, 2, None).await;

    let lock = client
        .acquire(Acquire::mutex("m").ttl(Duration::from_secs(2)))
        .await
        .unwrap();
    proxy.blackhole();
    tokio::time::sleep(Duration::from_secs(5)).await; // two and a half lease lengths of partition for the old connections
    assert!(
        !lock.is_lost(),
        "the renewals must have moved to fresh connections in time"
    );
    assert!(
        other
            .try_acquire(Acquire::mutex("m"))
            .await
            .unwrap()
            .is_none(),
        "still held"
    );
    lock.release().await.unwrap();
}

// ------------------------------------------------------------------------------ the pool

#[tokio::test]
async fn the_pool_refills_after_losing_one_connection_and_after_losing_all_of_them() {
    let srv = TestServer::start().await;
    let proxy = Proxy::start(srv.addr).await;
    let client = proxied_client(&proxy, 4, None).await;

    proxy.cut_one();
    // Requests keep working on the remaining connections (a request that hit the dead one is retried).
    for i in 0..10 {
        let lock = client
            .try_acquire(Acquire::mutex(format!("k{i}")))
            .await
            .unwrap();
        assert!(lock.is_some());
    }
    wait_for("the pool to refill", Duration::from_secs(5), || {
        client.connections() == 4
    })
    .await;
    assert_eq!(client.reconnects(), 1);

    let session = client.session_id();
    proxy.cut();
    wait_for(
        "every connection to come back",
        Duration::from_secs(5),
        || client.reconnects() >= 5 && client.connections() == 4,
    )
    .await;
    assert_eq!(
        client.session_id(),
        session,
        "the same session is rejoined, not recreated"
    );
}

#[tokio::test]
async fn connections_that_keep_dying_are_retried_with_backoff_not_in_a_tight_loop() {
    let srv = TestServer::start().await;
    let proxy = Proxy::start(srv.addr).await;
    let client = proxied_client(&proxy, 2, None).await;

    // From now on every connection is cut shortly after it is established (it gets through the
    // handshake first, which is what a tight loop would exploit).
    proxy.flap(ms(150));
    proxy.cut();
    let before = proxy.accepted();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let attempts = proxy.accepted() - before;
    println!("{attempts} connection attempts in 3 s with every connection flapping");
    assert!(attempts >= 2, "it should keep trying ({attempts})");
    assert!(
        attempts <= 20,
        "{attempts} connection attempts in 3 s: that is a reconnect storm"
    );
    drop(client);
}

// ------------------------------------------------------------------------------ explaining failures

fn free_addr() -> std::net::SocketAddr {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    probe.local_addr().unwrap()
}

#[tokio::test]
async fn a_server_that_comes_back_with_a_different_password_is_reported_as_an_authentication_failure()
 {
    let addr = free_addr();
    let first =
        TestServer::start_with_users(addr, &[("billing", "pw-billing", true)], |_| {}).await;
    let client = client_for(addr, "billing").connect().await.unwrap();
    first.stop().await;
    let _second =
        TestServer::start_with_users(addr, &[("billing", "rotated-password", true)], |_| {}).await;

    // The old session is gone, the client tries to log in again, and the server refuses it.
    let err = client.acquire(Acquire::mutex("m")).await.unwrap_err();
    assert!(
        matches!(err, Error::Auth(_)),
        "expected an authentication error, got {err:?}"
    );
}

#[tokio::test]
async fn a_server_that_stays_down_is_reported_as_unreachable_with_the_reason() {
    let addr = free_addr();
    let server = TestServer::start_with(addr, |_| {}).await;
    let client = client_for(addr, "billing").connect().await.unwrap();
    server.stop().await;

    let err = client.acquire(Acquire::mutex("m")).await.unwrap_err();
    match err {
        Error::Connect { reason, .. } => assert!(!reason.is_empty()),
        other => panic!("expected Connect with a reason, got {other:?}"),
    }
}
