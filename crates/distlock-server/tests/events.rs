//! End-to-end tests of events (named broadcasts) over real sockets and protobuf frames.

mod common;

use common::*;
use distlock_proto::v1 as pb;
use distlock_proto::v1::server_frame as sf;
use pb::ErrorCode;

#[tokio::test]
async fn a_set_wakes_waiters_in_different_sessions_and_reports_how_many() {
    let srv = TestServer::start().await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let (mut b, _) = Client::login(srv.addr, "reports").await;
    let (mut setter, _) = Client::login(srv.addr, "billing").await;

    expect_event_queued(a.event_wait(1, "deploy", 5_000, 0).await);
    expect_event_queued(b.event_wait(2, "deploy", 5_000, 0).await);
    a.expect_no_push(ms(100)).await;

    let ok = expect_event_set(setter.event_set("deploy", Some("v1.2"), Some(0)).await);
    assert_eq!(ok.woken, 2);
    assert_ne!(ok.publication_id, 0);
    assert_eq!(ok.publish_ms, 0);

    let (req_a, name_a, pub_a) = expect_published_push(a.expect_push().await);
    let (req_b, name_b, pub_b) = expect_published_push(b.expect_push().await);
    assert_eq!((req_a, name_a.as_str()), (1, "deploy"));
    assert_eq!((req_b, name_b.as_str()), (2, "deploy"));
    for p in [&pub_a, &pub_b] {
        assert_eq!(p.id, ok.publication_id);
        assert_eq!(p.value.as_deref(), Some("v1.2"));
        assert_eq!(p.age_ms, 0);
    }
    // Woken once: a second set has nobody to wake.
    assert_eq!(
        expect_event_set(setter.event_set("deploy", None, Some(0)).await).woken,
        0
    );
    a.expect_no_push(ms(100)).await;
}

#[tokio::test]
async fn only_waiters_of_that_name_are_woken() {
    let srv = TestServer::start().await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let (mut setter, _) = Client::login(srv.addr, "reports").await;
    expect_event_queued(a.event_wait(1, "one", 5_000, 0).await);
    assert_eq!(
        expect_event_set(setter.event_set("two", None, Some(0)).await).woken,
        0
    );
    a.expect_no_push(ms(150)).await;
}

#[tokio::test]
async fn a_wait_inside_the_publish_window_is_answered_at_once_and_after_it_waits_again() {
    let srv = TestServer::start().await;
    let (mut setter, _) = Client::login(srv.addr, "billing").await;
    let (mut late, _) = Client::login(srv.addr, "reports").await;

    let ok = expect_event_set(setter.event_set("e", Some("payload"), Some(400)).await);
    assert_eq!((ok.woken, ok.publish_ms), (0, 400));

    let p = expect_event_published(late.event_wait(1, "e", 5_000, 0).await);
    assert_eq!(p.id, ok.publication_id);
    assert_eq!(p.value.as_deref(), Some("payload"));

    tokio::time::sleep(ms(500)).await;
    expect_event_queued(late.event_wait(2, "e", 5_000, 0).await);
    // A later set reaches that waiter, with a newer id.
    let second = expect_event_set(setter.event_set("e", None, Some(0)).await);
    assert_eq!(second.woken, 1);
    assert!(second.publication_id > ok.publication_id);
    let (_, _, woken) = expect_published_push(late.expect_push().await);
    assert_eq!((woken.id, woken.value), (second.publication_id, None));
}

#[tokio::test]
async fn after_id_skips_a_publication_the_caller_has_already_seen() {
    let srv = TestServer::start().await;
    let (mut c, _) = Client::login(srv.addr, "billing").await;
    let first = expect_event_set(c.event_set("e", Some("1"), Some(10_000)).await);

    expect_event_queued(c.event_wait(1, "e", 5_000, first.publication_id).await);
    let second = expect_event_set(c.event_set("e", Some("2"), Some(10_000)).await);
    assert_eq!(second.woken, 1);
    let (_, _, p) = expect_published_push(c.expect_push().await);
    assert_eq!(
        (p.id, p.value.as_deref()),
        (second.publication_id, Some("2"))
    );
}

#[tokio::test]
async fn the_default_window_applies_and_a_longer_one_is_capped() {
    let srv = TestServer::start_with(|cfg| {
        cfg.lock.event_default_publish = ms(1_500);
        cfg.lock.event_max_publish = ms(3_000);
    })
    .await;
    let (mut c, _) = Client::login(srv.addr, "billing").await;
    assert_eq!(
        expect_event_set(c.event_set("a", None, None).await).publish_ms,
        1_500
    );
    assert_eq!(
        expect_event_set(c.event_set("b", None, Some(86_400_000)).await).publish_ms,
        3_000
    );
    assert_eq!(
        expect_event_set(c.event_set("c", None, Some(0)).await).publish_ms,
        0
    );
}

#[tokio::test]
async fn a_wait_times_out_with_a_push_and_a_zero_wait_is_a_poll() {
    let srv = TestServer::start().await;
    let (mut c, _) = Client::login(srv.addr, "billing").await;

    expect_event_queued(c.event_wait(4, "e", 300, 0).await);
    c.expect_no_push(ms(100)).await;
    match c.expect_push().await {
        sf::Body::WaitTimedOut(t) => assert_eq!((t.request_id, t.name.as_str()), (4, "e")),
        other => panic!("expected WaitTimedOut, got {other:?}"),
    }

    expect_error_code(c.event_wait(5, "e", 0, 0).await, ErrorCode::Busy);
    expect_event_set(c.event_set("e", None, Some(5_000)).await);
    expect_event_published(c.event_wait(6, "e", 0, 0).await);
}

#[tokio::test]
async fn cancel_dequeues_an_event_wait() {
    let srv = TestServer::start().await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let (mut setter, _) = Client::login(srv.addr, "reports").await;
    expect_event_queued(a.event_wait(9, "e", 5_000, 0).await);
    assert_eq!(a.cancel(9).await, pb::cancel_ok::Outcome::Dequeued);
    assert_eq!(a.cancel(9).await, pb::cancel_ok::Outcome::NotFound);
    assert_eq!(
        expect_event_set(setter.event_set("e", None, Some(0)).await).woken,
        0
    );
    a.expect_no_push(ms(150)).await;
}

#[tokio::test]
async fn a_wait_survives_its_connection_and_is_listed_and_delivered_on_another() {
    let srv = TestServer::start().await;
    let (mut setter, _) = Client::login(srv.addr, "reports").await;
    let (mut c1, hello) = Client::login(srv.addr, "billing").await;
    expect_event_queued(c1.event_wait(3, "e", 10_000, 0).await);
    drop(c1);
    tokio::time::sleep(ms(100)).await;

    // The wait belongs to the session, not the connection: reclaim lists it, and the wake-up
    // arrives on whichever connection of the session is attached.
    let (mut c2, attached) = Client::attach(srv.addr, &hello).await;
    let view = attached.view.expect("view");
    assert_eq!(view.pending_events.len(), 1);
    assert_eq!(
        (
            view.pending_events[0].request_id,
            view.pending_events[0].name.as_str()
        ),
        (3, "e")
    );
    assert!(view.held.is_empty() && view.pending.is_empty());

    assert_eq!(
        expect_event_set(setter.event_set("e", Some("x"), Some(0)).await).woken,
        1
    );
    let (request, _, p) = expect_published_push(c2.expect_push().await);
    assert_eq!((request, p.value.as_deref()), (3, Some("x")));
}

#[tokio::test]
async fn a_set_while_the_session_has_no_connection_is_not_lost_inside_its_window() {
    // The waiter's push is dropped (nobody is connected), but the publication lives on for its
    // window, so the client's re-wait after reconnecting still receives it.
    let srv = TestServer::start().await;
    let (mut setter, _) = Client::login(srv.addr, "reports").await;
    let (mut c1, hello) = Client::login(srv.addr, "billing").await;
    expect_event_queued(c1.event_wait(1, "e", 10_000, 0).await);
    drop(c1);
    tokio::time::sleep(ms(100)).await;

    let ok = expect_event_set(setter.event_set("e", Some("while-away"), Some(5_000)).await);
    assert_eq!(
        ok.woken, 1,
        "the table woke it, even though nobody could be told"
    );

    let (mut c2, attached) = Client::attach(srv.addr, &hello).await;
    assert!(
        attached.view.expect("view").pending_events.is_empty(),
        "no longer registered"
    );
    let p = expect_event_published(c2.event_wait(2, "e", 1_000, 0).await);
    assert_eq!(
        (p.id, p.value.as_deref()),
        (ok.publication_id, Some("while-away"))
    );
}

#[tokio::test]
async fn closing_a_session_drops_its_waits() {
    let srv = TestServer::start().await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let (mut setter, _) = Client::login(srv.addr, "reports").await;
    expect_event_queued(a.event_wait(1, "e", 5_000, 0).await);
    match a
        .call(pb::client_frame::Body::CloseSession(pb::CloseSession {}))
        .await
    {
        sf::Body::CloseSessionOk(_) => {}
        other => panic!("expected CloseSessionOk, got {other:?}"),
    }
    assert_eq!(
        expect_event_set(setter.event_set("e", None, Some(0)).await).woken,
        0
    );
}

#[tokio::test]
async fn requests_are_validated() {
    let srv = TestServer::start_with(|cfg| {
        cfg.lock.max_event_value_len = 8;
        cfg.lock.max_name_len = 16;
    })
    .await;
    let (mut c, _) = Client::login(srv.addr, "billing").await;

    expect_error_code(
        c.event_wait(1, "", 1_000, 0).await,
        ErrorCode::InvalidArgument,
    );
    expect_error_code(
        c.event_wait(1, &"n".repeat(17), 1_000, 0).await,
        ErrorCode::InvalidArgument,
    );
    expect_error_code(
        c.event_set("", None, None).await,
        ErrorCode::InvalidArgument,
    );
    expect_error_code(
        c.event_set("e", Some("123456789"), None).await,
        ErrorCode::InvalidArgument,
    );
    expect_event_set(c.event_set("e", Some("12345678"), None).await);
    // The connection is still usable after errors.
    expect_event_queued(c.event_wait(2, "other", 1_000, 0).await);
}

#[tokio::test]
async fn a_request_id_cannot_be_shared_between_a_lock_and_an_event_wait() {
    let srv = TestServer::start().await;
    let (mut c, _) = Client::login(srv.addr, "billing").await;
    expect_event_queued(c.event_wait(1, "e", 5_000, 0).await);
    expect_error_code(
        c.acquire(1, "m", pb::Mode::Mutex, 1_000, 0).await,
        ErrorCode::RequestConflict,
    );
    // Retrying the same wait is idempotent.
    expect_event_queued(c.event_wait(1, "e", 5_000, 0).await);
}

#[tokio::test]
async fn too_many_waiters_on_one_event_are_refused() {
    let srv = TestServer::start_with(|cfg| cfg.lock.max_waiters_per_lock = 2).await;
    let (mut c, _) = Client::login(srv.addr, "billing").await;
    expect_event_queued(c.event_wait(1, "e", 5_000, 0).await);
    expect_event_queued(c.event_wait(2, "e", 5_000, 0).await);
    expect_error_code(
        c.event_wait(3, "e", 5_000, 0).await,
        ErrorCode::ResourceExhausted,
    );
    expect_event_queued(c.event_wait(4, "elsewhere", 5_000, 0).await);
}
