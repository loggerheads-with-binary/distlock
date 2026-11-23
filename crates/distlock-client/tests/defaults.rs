//! Process-wide defaults (`distlock_client::defaults`) and how they cascade.
//!
//! The defaults are global, so every test here takes the same lock and resets them when it is done.

mod common;

use std::time::Duration;

use common::*;
use distlock_client::defaults::*;
use distlock_client::{
    Acquire, Error, EventSet, EventWait, MutexPrimitive, Primitive, SemaphorePrimitive,
    WriteLockPrimitive,
};
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::Instant;

static SERIAL: Mutex<()> = Mutex::const_new(());

/// Holds the turn, and clears every default on the way in and out (also when a test panics).
struct Turn(#[allow(dead_code)] MutexGuard<'static, ()>);

impl Turn {
    async fn take() -> Self {
        let guard = SERIAL.lock().await;
        reset_all();
        Turn(guard)
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        reset_all();
    }
}

#[tokio::test]
async fn default_wait_applies_when_a_request_sets_none() {
    let _turn = Turn::take().await;
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let _held = a.acquire(Acquire::mutex("job")).await.unwrap();

    set_default_wait(Some(ms(150)));
    let started = Instant::now();
    assert!(matches!(
        b.acquire(Acquire::mutex("job")).await,
        Err(Error::Timeout)
    ));
    assert!(started.elapsed() >= ms(150));

    // and for a primitive built before the default existed
    let job = MutexPrimitive::new("job");
    set_default_wait(Some(Duration::ZERO));
    assert!(matches!(job.bind(&b).await, Err(Error::Busy)));
}

#[tokio::test]
async fn an_explicit_wait_beats_every_default() {
    let _turn = Turn::take().await;
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let _held = a.acquire(Acquire::mutex("job")).await.unwrap();

    set_default_wait(Some(Duration::ZERO));
    set_default_mutex_wait(Some(Duration::ZERO));
    let started = Instant::now();
    let res = b.acquire(Acquire::mutex("job").wait(ms(200))).await;
    assert!(matches!(res, Err(Error::Timeout)), "{res:?}");
    assert!(
        started.elapsed() >= ms(200),
        "waited its own 200 ms, not the default"
    );
}

#[tokio::test]
async fn wait_forever_beats_a_default_wait() {
    let _turn = Turn::take().await;
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let _held = a.acquire(Acquire::mutex("job")).await.unwrap();

    set_default_wait(Some(ms(50)));
    let waiting = b.acquire(Acquire::mutex("job").wait_forever());
    assert!(
        tokio::time::timeout(ms(500), waiting).await.is_err(),
        "still waiting long after the 50 ms default"
    );
}

#[tokio::test]
async fn the_kind_default_beats_the_general_one() {
    let _turn = Turn::take().await;
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let _m = a.acquire(Acquire::mutex("m")).await.unwrap();
    let _s = a.acquire(Acquire::semaphore("s", 1, 1)).await.unwrap();

    set_default_wait(Some(Duration::ZERO)); // general: do not wait
    set_default_semaphore_wait(Some(ms(250))); // semaphores wait a little

    assert!(matches!(
        b.acquire(Acquire::mutex("m")).await,
        Err(Error::Busy)
    ));
    let started = Instant::now();
    let sem = SemaphorePrimitive::new("s", 1);
    assert!(matches!(sem.bind(&b).await, Err(Error::Timeout)));
    assert!(started.elapsed() >= ms(250));

    // resetting the kind default lets the general one apply again
    reset_default_semaphore_wait();
    assert!(matches!(sem.bind(&b).await, Err(Error::Busy)));
}

#[tokio::test]
async fn default_ttl_cascades_by_kind_and_falls_back_to_the_client() {
    let _turn = Turn::take().await;
    let srv = TestServer::start().await;
    let client = srv.client("billing"); // default_ttl: 30 s
    let c = client.connect().await.unwrap();

    // nothing set: the client's own default_ttl
    let l = c.acquire(Acquire::mutex("a")).await.unwrap();
    assert_eq!(l.ttl(), Duration::from_secs(30));
    l.release().await.unwrap();

    set_default_ttl(Duration::from_secs(20)).unwrap();
    set_default_write_ttl(Duration::from_secs(11)).unwrap();
    let m = MutexPrimitive::new("a").bind(&c).await.unwrap();
    let w = WriteLockPrimitive::new("w").bind(&c).await.unwrap();
    assert_eq!(m.ttl(), Duration::from_secs(20));
    assert_eq!(w.ttl(), Duration::from_secs(11));
    drop((m, w));

    // a request's own ttl still wins
    let own = MutexPrimitive::new("b")
        .ttl(Duration::from_secs(9))
        .bind(&c)
        .await
        .unwrap();
    assert_eq!(own.ttl(), Duration::from_secs(9));
}

#[tokio::test]
async fn invalid_default_ttls_are_rejected_at_once() {
    let _turn = Turn::take().await;
    assert!(matches!(
        set_default_ttl(Duration::ZERO),
        Err(Error::InvalidArgument(_))
    ));
    assert!(set_default_mutex_ttl(Duration::from_secs(u64::from(u32::MAX))).is_err());
    assert_eq!(effective_ttl(distlock_client::Kind::Mutex), None);
}

#[tokio::test]
async fn event_waits_use_the_event_default_then_the_general_one() {
    let _turn = Turn::take().await;
    let srv = TestServer::start().await;
    let c = srv.client("billing").connect().await.unwrap();

    set_default_wait(Some(ms(100)));
    let started = Instant::now();
    assert!(matches!(
        c.wait_event(EventWait::new("never")).await,
        Err(Error::Timeout)
    ));
    assert!(started.elapsed() >= ms(100));

    set_default_event_wait(Some(Duration::ZERO));
    assert!(matches!(
        c.wait_event(EventWait::new("never")).await,
        Err(Error::Timeout)
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
    // explicit "forever" is still possible
    let forever = c.wait_event(EventWait::new("never").wait_forever());
    assert!(tokio::time::timeout(ms(300), forever).await.is_err());
}

#[tokio::test]
async fn default_publish_window_applies_to_sets_without_one() {
    let _turn = Turn::take().await;
    let srv = TestServer::start().await;
    let c = srv.client("billing").connect().await.unwrap();

    set_default_event_publish_for(Duration::from_secs(2));
    let set = c.set_event(EventSet::new("e")).await.unwrap();
    assert_eq!(set.publish_for, Duration::from_secs(2));
    let own = c
        .set_event(EventSet::new("e").publish_for(Duration::from_secs(1)))
        .await
        .unwrap();
    assert_eq!(own.publish_for, Duration::from_secs(1));
}

#[tokio::test]
async fn reset_all_restores_the_old_behaviour() {
    let _turn = Turn::take().await;
    set_default_wait(Some(Duration::ZERO));
    set_default_ttl(Duration::from_secs(1)).unwrap();
    set_default_event_publish_for(Duration::from_secs(1));
    reset_all();
    let kind = distlock_client::Kind::Mutex;
    assert_eq!(effective_wait(kind), None);
    assert_eq!(effective_ttl(kind), None);
    assert_eq!(effective_event_wait(), None);
    assert_eq!(effective_event_publish_for(), None);
}
