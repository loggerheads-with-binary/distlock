//! Events through the Rust client: async and blocking, against the real server.

mod common;

use std::time::Duration;

use common::*;
use distlock_client::{Acquire, Error, EventSet, EventWait};
use tokio::time::Instant;

fn set(name: &str) -> EventSet {
    EventSet::new(name)
}

fn wait(name: &str, secs: u64) -> EventWait {
    EventWait::new(name).wait(Duration::from_secs(secs))
}

/// Poll until the server reports `n` waiters would be woken, so a set made next cannot race the
/// waiters' registration. A set with a zero window and a throwaway name would disturb nothing, but
/// there is no "how many are waiting" query, so tests sleep briefly instead.
async fn registered() {
    tokio::time::sleep(ms(200)).await;
}

#[tokio::test]
async fn many_clients_wait_and_one_set_wakes_them_all_with_the_value() {
    let srv = TestServer::start().await;
    let setter = srv.client("billing").connect().await.unwrap();
    let mut waiters = Vec::new();
    for user in ["billing", "reports", "billing"] {
        let client = srv.client(user).connect().await.unwrap();
        waiters.push(tokio::spawn(async move {
            client.wait_event(wait("deploy", 10)).await
        }));
    }
    registered().await;

    let result = setter
        .set_event(set("deploy").value("v1.2.3").publish_for(Duration::ZERO))
        .await
        .unwrap();
    assert_eq!(result.woken, 3, "the number of clients that were waiting");
    assert_eq!(result.publish_for, Duration::ZERO);

    for waiter in waiters {
        let published = waiter.await.unwrap().unwrap();
        assert_eq!(published.value.as_deref(), Some("v1.2.3"));
        assert_eq!(published.id, result.id);
        assert_eq!(published.age, Duration::ZERO);
    }
}

#[tokio::test]
async fn one_client_can_have_many_waits_on_the_same_event() {
    let srv = TestServer::start().await;
    let client = srv.client("billing").connect().await.unwrap();
    let tasks: Vec<_> = (0..5)
        .map(|_| {
            let client = client.clone();
            tokio::spawn(async move { client.wait_event(wait("e", 10)).await })
        })
        .collect();
    registered().await;
    assert_eq!(
        client
            .set_event(set("e").publish_for(Duration::ZERO))
            .await
            .unwrap()
            .woken,
        5
    );
    for task in tasks {
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn a_set_without_a_value_gives_none_and_an_empty_value_gives_empty() {
    let srv = TestServer::start().await;
    let client = srv.client("billing").connect().await.unwrap();
    client.set_event(set("none")).await.unwrap();
    client.set_event(set("empty").value("")).await.unwrap();
    assert_eq!(
        client.wait_event(wait("none", 1)).await.unwrap().value,
        None
    );
    assert_eq!(
        client
            .wait_event(wait("empty", 1))
            .await
            .unwrap()
            .value
            .as_deref(),
        Some("")
    );
}

#[tokio::test]
async fn a_late_waiter_inside_the_window_gets_the_value_and_one_after_it_waits() {
    let srv = TestServer::start().await;
    let client = srv.client("billing").connect().await.unwrap();
    let result = client
        .set_event(set("e").value("kept").publish_for(ms(600)))
        .await
        .unwrap();
    assert_eq!((result.woken, result.publish_for), (0, ms(600)));

    let started = Instant::now();
    let early = client.wait_event(wait("e", 5)).await.unwrap();
    assert!(
        started.elapsed() < ms(400),
        "answered at once, not after waiting"
    );
    assert_eq!(
        (early.id, early.value.as_deref()),
        (result.id, Some("kept"))
    );
    assert!(early.age < ms(600));

    tokio::time::sleep(ms(700)).await;
    let late = client.wait_event(EventWait::new("e").wait(ms(300))).await;
    assert!(
        matches!(late, Err(Error::Timeout)),
        "the event is gone: {late:?}"
    );
}

#[tokio::test]
async fn the_default_window_comes_from_the_server_and_long_ones_are_capped() {
    let srv = TestServer::start_with("127.0.0.1:0".parse().unwrap(), |cfg| {
        cfg.lock.event_default_publish = ms(2_500);
        cfg.lock.event_max_publish = ms(4_000);
    })
    .await;
    let client = srv.client("billing").connect().await.unwrap();
    assert_eq!(
        client.set_event(set("a")).await.unwrap().publish_for,
        ms(2_500)
    );
    assert_eq!(
        client
            .set_event(set("b").publish_for(Duration::from_secs(3600)))
            .await
            .unwrap()
            .publish_for,
        ms(4_000)
    );
}

#[tokio::test]
async fn a_wait_times_out_with_timeout_and_a_zero_wait_only_checks() {
    let srv = TestServer::start().await;
    let client = srv.client("billing").connect().await.unwrap();

    let started = Instant::now();
    let res = client.wait_event(EventWait::new("e").wait(ms(300))).await;
    assert!(matches!(res, Err(Error::Timeout)), "{res:?}");
    assert!(started.elapsed() >= ms(250));
    assert!(
        started.elapsed() < ms(2_000),
        "the server must be told how long to wait, not left to the client's fallback"
    );

    let res = client
        .wait_event(EventWait::new("e").wait(Duration::ZERO))
        .await;
    assert!(
        matches!(res, Err(Error::Timeout)),
        "nothing published: {res:?}"
    );
    client
        .set_event(set("e").value("x").publish_for(Duration::from_secs(5)))
        .await
        .unwrap();
    let hit = client
        .wait_event(EventWait::new("e").wait(Duration::ZERO))
        .await
        .unwrap();
    assert_eq!(hit.value.as_deref(), Some("x"));
}

#[tokio::test]
async fn dropping_a_wait_withdraws_it_so_the_next_set_does_not_count_it() {
    let srv = TestServer::start().await;
    let client = srv.client("billing").connect().await.unwrap();
    let stays = {
        let client = client.clone();
        tokio::spawn(async move { client.wait_event(wait("e", 10)).await })
    };
    let goes = {
        let client = client.clone();
        tokio::spawn(async move { client.wait_event(wait("e", 10)).await })
    };
    registered().await;
    goes.abort();
    let _ = goes.await;
    registered().await;

    let result = client
        .set_event(set("e").publish_for(Duration::ZERO))
        .await
        .unwrap();
    assert_eq!(result.woken, 1, "only the wait that is still alive");
    stays.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_consumer_loop_skips_what_it_has_handled_using_after() {
    let srv = TestServer::start().await;
    let client = srv.client("billing").connect().await.unwrap();
    let first = client
        .set_event(set("tick").value("1").publish_for(Duration::from_secs(30)))
        .await
        .unwrap();

    // Without `after`, the same publication would come back every time for 30 s.
    let seen = client.wait_event(wait("tick", 1)).await.unwrap();
    assert_eq!(seen.id, first.id);
    let consumer = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .wait_event(
                    EventWait::new("tick")
                        .wait(Duration::from_secs(10))
                        .after(seen.id),
                )
                .await
        })
    };
    registered().await;
    assert!(
        !consumer.is_finished(),
        "the handled publication must not satisfy it"
    );

    let second = client.set_event(set("tick").value("2")).await.unwrap();
    assert_eq!(second.woken, 1);
    let next = consumer.await.unwrap().unwrap();
    assert_eq!((next.id, next.value.as_deref()), (second.id, Some("2")));
}

#[tokio::test]
async fn namespaces_and_families_keep_events_apart() {
    let srv = TestServer::start().await;
    let client = srv.client("billing").connect().await.unwrap();

    client
        .set_event(
            set("x")
                .namespace("a")
                .value("in-a")
                .publish_for(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    let res = client
        .wait_event(EventWait::new("x").namespace("b").wait(ms(200)))
        .await;
    assert!(
        matches!(res, Err(Error::Timeout)),
        "another namespace: {res:?}"
    );
    let hit = client
        .wait_event(EventWait::new("x").namespace("a").wait(ms(200)))
        .await
        .unwrap();
    assert_eq!(hit.value.as_deref(), Some("in-a"));

    // A lock called `x` and an event called `x` do not interact in either direction.
    let lock = client
        .acquire(Acquire::mutex("x").namespace("a"))
        .await
        .unwrap();
    client.set_event(set("x").namespace("a")).await.unwrap();
    assert!(lock.fencing_token() > 0);
    lock.release().await.unwrap();
}

#[tokio::test]
async fn invalid_requests_are_refused_before_or_by_the_server() {
    let srv = TestServer::start_with("127.0.0.1:0".parse().unwrap(), |cfg| {
        cfg.lock.max_event_value_len = 4;
    })
    .await;
    let client = srv.client("billing").connect().await.unwrap();
    assert!(matches!(
        client.wait_event(EventWait::new("")).await,
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        client.set_event(set("")).await,
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        client.set_event(set("e").namespace("bad:ns")).await,
        Err(Error::InvalidArgument(_))
    ));
    assert!(
        matches!(
            client.set_event(set("e").value("12345")).await,
            Err(Error::InvalidArgument(_))
        ),
        "too long for this server"
    );
    assert!(client.set_event(set("e").value("1234")).await.is_ok());
}

#[tokio::test]
async fn a_session_that_only_waits_is_not_expired_while_it_waits() {
    let srv = TestServer::start_with("127.0.0.1:0".parse().unwrap(), |cfg| {
        cfg.session_idle_timeout = ms(200);
    })
    .await;
    let waiter = srv.client("billing").connect().await.unwrap();
    let setter = srv.client("reports").connect().await.unwrap();
    let task = {
        let waiter = waiter.clone();
        tokio::spawn(async move { waiter.wait_event(wait("e", 10)).await })
    };
    tokio::time::sleep(ms(1_200)).await; // several idle timeouts; the wait has to survive them
    assert_eq!(
        setter
            .set_event(set("e").publish_for(Duration::ZERO))
            .await
            .unwrap()
            .woken,
        1
    );
    task.await.unwrap().unwrap();
}

// ------------------------------------------------------------------------------ blocking

#[test]
fn the_blocking_client_waits_and_sets_from_threads() {
    let (_rt, addr) = start_in_background();
    let setter = client_for(addr, "billing").connect_blocking().unwrap();
    let threads: Vec<_> = ["billing", "reports"]
        .into_iter()
        .map(|user| {
            let client = client_for(addr, user).connect_blocking().unwrap();
            std::thread::spawn(move || client.wait_event(wait("go", 10)))
        })
        .collect();
    std::thread::sleep(ms(400));

    let result = setter
        .set_event(set("go").value("now").publish_for(Duration::ZERO))
        .unwrap();
    assert_eq!(result.woken, 2);
    for t in threads {
        assert_eq!(t.join().unwrap().unwrap().value.as_deref(), Some("now"));
    }
    assert!(matches!(
        setter.wait_event(EventWait::new("go").wait(ms(100))),
        Err(Error::Timeout)
    ));
}

// ------------------------------------------------------------------------------ bad networks

async fn wait_for(what: &str, within: Duration, mut condition: impl FnMut() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(started.elapsed() < within, "gave up waiting for: {what}");
        tokio::time::sleep(ms(25)).await;
    }
}

fn any_port() -> std::net::SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// A waiter behind a proxy, and a setter that talks to the server directly.
async fn waiter_and_setter(
    srv: &TestServer,
    proxy: &Proxy,
) -> (distlock_client::Client, distlock_client::Client) {
    let waiter = distlock_client::DistLockClient {
        pool_size: 2,
        ..client_for(proxy.addr, "billing")
    }
    .connect()
    .await
    .unwrap();
    wait_for("the pool to come up", Duration::from_secs(5), || {
        waiter.connections() == 2
    })
    .await;
    (waiter, srv.client("reports").connect().await.unwrap())
}

#[tokio::test]
async fn a_wait_survives_every_connection_being_cut_and_is_woken_afterwards() {
    let srv = TestServer::start_with(any_port(), |_| {}).await;
    let proxy = Proxy::start(srv.addr).await;
    let (waiter, setter) = waiter_and_setter(&srv, &proxy).await;

    let task = {
        let waiter = waiter.clone();
        tokio::spawn(async move { waiter.wait_event(wait("e", 20)).await })
    };
    registered().await;
    proxy.cut();
    wait_for("the pool to reconnect", Duration::from_secs(10), || {
        waiter.reconnects() >= 2 && waiter.connections() == 2
    })
    .await;
    registered().await;
    assert!(!task.is_finished());

    // The server still had the wait, so this wakes it: reclaim kept it alive.
    let result = setter
        .set_event(set("e").value("after-cut").publish_for(Duration::ZERO))
        .await
        .unwrap();
    assert_eq!(result.woken, 1);
    let published = task.await.unwrap().unwrap();
    assert_eq!(published.value.as_deref(), Some("after-cut"));
}

#[tokio::test]
async fn a_set_made_while_the_waiter_is_offline_still_reaches_it_inside_the_publish_window() {
    let srv = TestServer::start_with(any_port(), |_| {}).await;
    let proxy = Proxy::start(srv.addr).await;
    let (waiter, setter) = waiter_and_setter(&srv, &proxy).await;

    let task = {
        let waiter = waiter.clone();
        tokio::spawn(async move { waiter.wait_event(wait("e", 20)).await })
    };
    registered().await;
    proxy.pause();
    proxy.cut();
    wait_for(
        "the waiter to lose its connections",
        Duration::from_secs(5),
        || waiter.connections() == 0,
    )
    .await;

    let result = setter
        .set_event(
            set("e")
                .value("while-away")
                .publish_for(Duration::from_secs(10)),
        )
        .await
        .unwrap();
    assert_eq!(
        result.woken, 1,
        "the server woke it, though nobody could be told"
    );
    assert!(!task.is_finished());

    proxy.resume();
    let published = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the waiter should be answered after it reconnects")
        .unwrap()
        .unwrap();
    assert_eq!(published.value.as_deref(), Some("while-away"));
    assert_eq!(published.id, result.id);
    assert!(
        published.age > Duration::ZERO,
        "it came from the window, not the wake-up"
    );
}

#[tokio::test]
async fn a_set_with_no_window_made_while_offline_is_missed_and_the_next_one_arrives() {
    // The documented limit: with a zero window there is no memory, so a wake-up that could not be
    // delivered is gone. The waiter keeps waiting and receives the next set.
    let srv = TestServer::start_with(any_port(), |_| {}).await;
    let proxy = Proxy::start(srv.addr).await;
    let (waiter, setter) = waiter_and_setter(&srv, &proxy).await;
    let task = {
        let waiter = waiter.clone();
        tokio::spawn(async move { waiter.wait_event(wait("e", 20)).await })
    };
    registered().await;
    proxy.pause();
    proxy.cut();
    wait_for(
        "the waiter to lose its connections",
        Duration::from_secs(5),
        || waiter.connections() == 0,
    )
    .await;
    setter
        .set_event(set("e").value("lost").publish_for(Duration::ZERO))
        .await
        .unwrap();
    proxy.resume();
    wait_for("the waiter to reconnect", Duration::from_secs(10), || {
        waiter.connections() == 2
    })
    .await;
    tokio::time::sleep(ms(600)).await;
    assert!(!task.is_finished(), "nothing to deliver: it keeps waiting");

    let next = setter
        .set_event(set("e").value("next").publish_for(Duration::ZERO))
        .await
        .unwrap();
    assert_eq!(next.woken, 1);
    assert_eq!(task.await.unwrap().unwrap().value.as_deref(), Some("next"));
}

#[tokio::test]
async fn a_wait_that_expires_while_offline_ends_on_time_instead_of_starting_over() {
    let srv = TestServer::start_with(any_port(), |_| {}).await;
    let proxy = Proxy::start(srv.addr).await;
    let (waiter, _setter) = waiter_and_setter(&srv, &proxy).await;

    let task = {
        let waiter = waiter.clone();
        tokio::spawn(async move { waiter.wait_event(EventWait::new("e").wait(ms(3_000))).await })
    };
    registered().await;
    proxy.pause();
    proxy.cut();
    wait_for(
        "the waiter to lose its connections",
        Duration::from_secs(5),
        || waiter.connections() == 0,
    )
    .await;
    tokio::time::sleep(ms(3_300)).await; // the server times the wait out while nobody is listening
    assert!(
        !task.is_finished(),
        "it cannot learn that until it reconnects"
    );
    proxy.resume();

    // How long the reconnect takes is up to the client's backoff. What matters is that once it is
    // back and learns the wait is gone, the wait ends at once: its budget ran out while it was
    // away, and registering again must not give it a fresh one.
    wait_for("the pool to reconnect", Duration::from_secs(15), || {
        waiter.connections() == 2
    })
    .await;
    let res = tokio::time::timeout(ms(1_500), task)
        .await
        .expect("the wait should end right after the reconnect, not run its budget again")
        .unwrap();
    assert!(matches!(res, Err(Error::Timeout)), "{res:?}");
}
