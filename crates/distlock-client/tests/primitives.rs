//! Primitives: one client-less definition, bound again and again, to one client or to several.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use distlock_client::{
    Attach, AttachBlocking, Error, EventSet, EventWait, Kind, MutexPrimitive, Primitive,
    ReadLockPrimitive, SemaphorePrimitive, State, WriteLockPrimitive,
};

#[tokio::test]
async fn one_primitive_binds_repeatedly_to_the_same_client() {
    let srv = TestServer::start().await;
    let client = srv.client("billing").connect().await.unwrap();
    let job = MutexPrimitive::new("job").ttl(Duration::from_secs(5));

    let mut tokens = Vec::new();
    for _ in 0..3 {
        let lock = job.bind(&client).await.unwrap();
        assert_eq!((lock.name(), lock.kind()), ("job", Kind::Mutex));
        assert_eq!(lock.ttl(), Duration::from_secs(5));
        tokens.push(lock.fencing_token());
        lock.release().await.unwrap();
    }
    assert!(
        tokens.windows(2).all(|w| w[0] < w[1]),
        "each bind is a fresh grant"
    );
}

#[tokio::test]
async fn one_primitive_binds_to_different_clients() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let job = MutexPrimitive::new("job");

    let held = job.bind(&a).await.unwrap();
    assert!(
        job.try_bind(&b).await.unwrap().is_none(),
        "taken by the other client"
    );
    assert!(matches!(
        MutexPrimitive::new("job")
            .wait(Duration::from_millis(100))
            .bind(&b)
            .await,
        Err(Error::Timeout)
    ));
    held.release_with(Some(State::Succeeded), None)
        .await
        .unwrap();

    let next = job.bind(&b).await.unwrap();
    assert_eq!(next.previous().unwrap().state, State::Succeeded);
}

#[tokio::test]
async fn attach_is_the_same_as_bind_and_works_through_an_arc() {
    let srv = TestServer::start().await;
    let client = srv.client("billing").connect().await.unwrap();
    let shared = Arc::new(srv.client("reports").connect().await.unwrap());
    let job = MutexPrimitive::new("job");

    let lock = client.attach(&job).await.unwrap();
    assert!(
        matches!(
            MutexPrimitive::new("job")
                .wait(Duration::from_millis(80))
                .bind(shared.as_ref())
                .await,
            Err(Error::Timeout)
        ),
        "held by the first client"
    );
    lock.release().await.unwrap();
    let lock = shared.attach(&job).await.unwrap();
    assert_eq!(lock.name(), "job");
}

#[tokio::test]
async fn read_write_and_mutex_primitives_keep_their_semantics() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let (r, w) = (
        ReadLockPrimitive::new("doc"),
        WriteLockPrimitive::new("doc"),
    );

    let r1 = r.bind(&a).await.unwrap();
    let r2 = r.bind(&b).await.unwrap(); // readers share
    assert!(w.try_bind(&b).await.unwrap().is_none());
    r1.release().await.unwrap();
    r2.release().await.unwrap();
    let w1 = w.bind(&a).await.unwrap();
    assert!(r.try_bind(&b).await.unwrap().is_none());
    // a mutex of the same name is a different family
    let m = MutexPrimitive::new("doc").bind(&b).await.unwrap();
    assert_eq!((w1.kind(), m.kind()), (Kind::Write, Kind::Mutex));
}

#[tokio::test]
async fn semaphore_primitive_counts_permits() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let sem = SemaphorePrimitive::new("pool", 2);
    let one = sem.bind(&a).await.unwrap();
    let two = sem.bind(&a).await.unwrap();
    assert!(
        sem.try_bind(&a).await.unwrap().is_none(),
        "both permits are out"
    );
    drop(one);
    // dropping releases in the background
    let three = sem
        .clone()
        .wait(Duration::from_secs(2))
        .bind(&a)
        .await
        .unwrap();
    drop((two, three));
    assert!(matches!(
        SemaphorePrimitive::new("bad", 1).permits(2).bind(&a).await,
        Err(Error::InvalidArgument(_))
    ));
}

#[tokio::test]
async fn namespace_on_a_primitive_separates_names() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let in_x = MutexPrimitive::new("n")
        .namespace("x")
        .bind(&a)
        .await
        .unwrap();
    let in_y = MutexPrimitive::new("n")
        .namespace("y")
        .try_bind(&a)
        .await
        .unwrap();
    assert!(in_y.is_some());
    assert!(
        MutexPrimitive::new("n")
            .namespace("x")
            .try_bind(&a)
            .await
            .unwrap()
            .is_none()
    );
    drop(in_x);
}

#[tokio::test]
async fn event_requests_are_primitives_too() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let waiting = EventWait::new("go").wait(Duration::from_secs(5));
    let setting = EventSet::new("go")
        .value("now")
        .publish_for(Duration::from_secs(1));

    let waiter = {
        let waiting = waiting.clone();
        tokio::spawn(async move { waiting.bind(&b).await })
    };
    tokio::time::sleep(ms(200)).await;
    let set = a.attach(&setting).await.unwrap();
    assert_eq!(set.woken, 1);
    assert_eq!(waiter.await.unwrap().unwrap().value.as_deref(), Some("now"));
}

#[test]
fn blocking_clients_attach_primitives_without_async() {
    let (_rt, addr) = start_in_background();
    let a = client_for(addr, "billing").connect_blocking().unwrap();
    let b = client_for(addr, "reports").connect_blocking().unwrap();
    let job = MutexPrimitive::new("job");

    let lock = AttachBlocking::attach(&a, &job).unwrap();
    assert!(job.try_bind_blocking(&b).unwrap().is_none());
    lock.release().unwrap();
    let lock = job.bind_blocking(&b).unwrap();
    assert_eq!(lock.name(), "job");
}
