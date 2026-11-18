//! End-to-end tests: a real server on a loopback socket, real protobuf frames, real bcrypt (cost 4).
//! Time-based behaviour uses real, short durations; assertions on timing are lower bounds only
//! ("not before"), so a slow machine can make a test slower but not flaky.

mod common;

use std::time::Duration;

use common::*;
use distlock_proto::v1 as pb;
use distlock_proto::v1::{client_frame as cf, server_frame as sf};
use pb::{ErrorCode, HolderState, Mode};
use tokio::time::Instant;

// ------------------------------------------------------------------------ handshake and auth

#[tokio::test]
async fn hello_opens_a_session_advertises_limits_and_ping_works() {
    let srv = TestServer::start().await;
    let (mut c, ok) = Client::login(srv.addr, "billing").await;
    assert_ne!(ok.session_id, 0);
    assert_eq!(ok.session_token.len(), 32);
    assert_eq!(ok.max_ttl_ms, 60_000);
    assert_eq!(ok.max_wait_ms, 300_000);
    assert_eq!(ok.max_frame_bytes, 1_048_576);
    assert_eq!(ok.ping_interval_ms, 10_000);
    match c.call(cf::Body::Ping(pb::Ping { nonce: 99 })).await {
        sf::Body::Pong(p) => assert_eq!(p.nonce, 99),
        other => panic!("expected Pong, got {other:?}"),
    }
}

#[tokio::test]
async fn two_logins_with_one_client_id_are_two_independent_sessions() {
    let srv = TestServer::start().await;
    let (mut pod1, ok1) = Client::login(srv.addr, "billing").await;
    let (mut pod2, ok2) = Client::login(srv.addr, "billing").await;
    assert_ne!(ok1.session_id, ok2.session_id);
    expect_granted(pod1.acquire(1, "m", Mode::Mutex, 5_000, 0).await);
    // Same application name, but a different owner: it must NOT get the lock.
    expect_error_code(
        pod2.acquire(1, "m", Mode::Mutex, 5_000, 0).await,
        ErrorCode::Busy,
    );
}

#[tokio::test]
async fn bad_credentials_are_rejected_identically_and_the_connection_is_closed() {
    let srv = TestServer::start().await;
    let mut messages = Vec::new();
    for (user, password) in [
        ("billing", "wrong"),
        ("billing", ""),
        ("nobody", "pw-billing"),
        ("retired", "pw-retired"), // disabled user with the right password
    ] {
        let mut c = Client::connect(srv.addr).await;
        let e = expect_error_code(
            c.call(hello_body(user, password)).await,
            ErrorCode::Unauthenticated,
        );
        messages.push(e.message);
        c.expect_closed().await;
    }
    assert!(
        messages.iter().all(|m| m == &messages[0]),
        "the error text must not reveal which part was wrong: {messages:?}"
    );
}

#[tokio::test]
async fn the_first_message_must_be_hello_or_attach() {
    let srv = TestServer::start().await;
    let mut c = Client::connect(srv.addr).await;
    expect_error_code(
        c.acquire(1, "m", Mode::Mutex, 1_000, 0).await,
        ErrorCode::InvalidArgument,
    );
    c.expect_closed().await;
}

#[tokio::test]
async fn protocol_violations_are_answered_and_close_the_connection() {
    let srv = TestServer::start().await;

    // Unsupported version.
    let mut c = Client::connect(srv.addr).await;
    let body = cf::Body::Hello(pb::Hello {
        protocol_version: 2,
        client_id: "billing".into(),
        password: "pw-billing".into(),
        instance_label: String::new(),
    });
    expect_error_code(c.call(body).await, ErrorCode::UnsupportedVersion);
    c.expect_closed().await;

    // seq 0 is reserved for server pushes.
    let mut c = Client::connect(srv.addr).await;
    c.send_with_seq(0, hello_body("billing", "pw-billing"))
        .await;
    match c.recv(WAIT).await {
        Recv::Frame(f) => assert!(matches!(f.body, Some(sf::Body::Error(_)))),
        _ => panic!("expected an error frame"),
    }
    c.expect_closed().await;

    // Not protobuf at all.
    let mut c = Client::connect(srv.addr).await;
    c.send_raw(vec![0xff; 32]).await;
    match c.recv(WAIT).await {
        Recv::Frame(f) => assert!(matches!(f.body, Some(sf::Body::Error(_)))),
        _ => panic!("expected an error frame"),
    }
    c.expect_closed().await;

    // Hello again after logging in.
    let (mut c, _) = Client::login(srv.addr, "billing").await;
    expect_error_code(
        c.call(hello_body("billing", "pw-billing")).await,
        ErrorCode::InvalidArgument,
    );
    c.expect_closed().await;
}

#[tokio::test]
async fn silent_connections_are_dropped_at_the_handshake_timeout() {
    let srv = TestServer::start_with(|c| c.handshake_timeout = ms(200)).await;
    let mut c = Client::connect(srv.addr).await;
    c.expect_closed().await;
}

#[tokio::test]
async fn frames_over_the_size_limit_close_the_connection() {
    let srv = TestServer::start_with(|c| c.max_frame_bytes = 4096).await;
    let (mut c, ok) = Client::login(srv.addr, "billing").await;
    assert_eq!(ok.max_frame_bytes, 4096);
    c.send_raw(vec![0u8; 10_000]).await;
    c.expect_closed().await;
}

#[tokio::test]
async fn idle_connections_are_closed_but_pings_keep_them_alive() {
    let srv = TestServer::start_with(|c| {
        c.ping_interval = ms(100);
        c.idle_connection_timeout = ms(300);
    })
    .await;
    let (mut c, _) = Client::login(srv.addr, "billing").await;
    for nonce in 0..8 {
        tokio::time::sleep(ms(100)).await; // 800ms in total, well past the 300ms idle timeout
        match c.call(cf::Body::Ping(pb::Ping { nonce })).await {
            sf::Body::Pong(p) => assert_eq!(p.nonce, nonce),
            other => panic!("expected Pong, got {other:?}"),
        }
    }
    c.expect_closed().await; // now go silent
}

#[tokio::test]
async fn a_client_that_stops_reading_is_cut_off_without_stalling_anyone_else() {
    const CAP: u64 = 1_000_000;
    let srv = TestServer::start().await;
    let (mut holder, _) = Client::login(srv.addr, "reports").await;
    expect_granted(holder.acquire(1, "m", Mode::Mutex, 30_000, 0).await);

    // Each try-lock is answered with a Busy error. This client sends them and never reads a reply,
    // so the server's outbound queue for it eventually fills up.
    let (mut slow, _) = Client::login(srv.addr, "billing").await;
    let mut sent = 0;
    while sent < CAP && slow.try_send_acquire(100 + sent, "m").await.is_ok() {
        sent += 1;
    }
    assert!(
        sent < CAP,
        "the server never cut off a client that does not read"
    );

    // Everyone else is unaffected: the engine never waited on the slow connection.
    let (mut other, _) = Client::login(srv.addr, "billing").await;
    expect_error_code(
        other.acquire(1, "m", Mode::Mutex, 1_000, 0).await,
        ErrorCode::Busy,
    );
    slow.expect_closed().await;
}

#[tokio::test]
async fn the_connection_limit_refuses_extra_connections() {
    let srv = TestServer::start_with(|c| c.max_connections = 2).await;
    let (_c1, _) = Client::login(srv.addr, "billing").await;
    let (_c2, _) = Client::login(srv.addr, "reports").await;
    let mut c3 = Client::connect(srv.addr).await;
    c3.expect_closed().await;
}

// ------------------------------------------------------------------------ locks over the wire

#[tokio::test]
async fn a_queued_acquire_is_granted_on_release_and_the_state_tag_flows_to_the_next_holder() {
    let srv = TestServer::start().await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let (mut b, _) = Client::login(srv.addr, "reports").await;

    let ga = expect_granted(a.acquire(1, "job", Mode::Mutex, 5_000, 0).await);
    assert!(ga.previous.is_none());
    assert_eq!(ga.name, "job");
    assert_eq!(ga.request_id, 1);
    expect_queued(b.acquire(1, "job", Mode::Mutex, 5_000, 5_000).await);

    expect_ok_release(
        a.release("job", ga.fencing_token, HolderState::Succeeded)
            .await,
    );

    let gb = expect_granted_push(b.expect_push().await);
    assert_eq!((gb.request_id, gb.name.as_str()), (1, "job"));
    assert!(gb.fencing_token > ga.fencing_token);
    let previous = gb
        .previous
        .expect("the next holder sees the last holder's tag");
    assert_eq!(previous.state, HolderState::Succeeded as i32);
    assert_eq!(previous.set_by_token, ga.fencing_token);
}

#[tokio::test]
async fn try_lock_is_busy_and_a_queued_acquire_times_out_with_a_push() {
    let srv = TestServer::start().await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let (mut b, _) = Client::login(srv.addr, "reports").await;
    expect_granted(a.acquire(1, "m", Mode::Mutex, 10_000, 0).await);

    expect_error_code(
        b.acquire(1, "m", Mode::Mutex, 5_000, 0).await,
        ErrorCode::Busy,
    );

    expect_queued(b.acquire(2, "m", Mode::Mutex, 5_000, 200).await);
    match b.next_push(WAIT).await.expect("a timeout notice") {
        sf::Body::WaitTimedOut(t) => assert_eq!((t.request_id, t.name.as_str()), (2, "m")),
        other => panic!("expected WaitTimedOut, got {other:?}"),
    }
}

#[tokio::test]
async fn readers_share_and_a_waiting_writer_gets_its_turn() {
    let srv = TestServer::start().await;
    let (mut r1, _) = Client::login(srv.addr, "billing").await;
    let (mut r2, _) = Client::login(srv.addr, "billing").await;
    let (mut w, _) = Client::login(srv.addr, "reports").await;

    let g1 = expect_granted(r1.acquire(1, "rw", Mode::Read, 5_000, 0).await);
    let g2 = expect_granted(r2.acquire(1, "rw", Mode::Read, 5_000, 0).await);
    expect_queued(w.acquire(1, "rw", Mode::Write, 5_000, 5_000).await);

    expect_ok_release(r1.release("rw", g1.fencing_token, HolderState::Unset).await);
    w.expect_no_push(ms(150)).await; // one reader still holds it
    expect_ok_release(r2.release("rw", g2.fencing_token, HolderState::Unset).await);
    expect_granted_push(w.expect_push().await);
}

#[tokio::test]
async fn semaphores_enforce_capacity_and_report_a_capacity_mismatch() {
    let srv = TestServer::start().await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let (mut b, _) = Client::login(srv.addr, "billing").await;
    let (mut c, _) = Client::login(srv.addr, "reports").await;

    let ga = expect_granted(a.acquire_semaphore(1, "sem", 2, 1, 5_000, 0).await);
    expect_granted(b.acquire_semaphore(1, "sem", 2, 1, 5_000, 0).await);
    expect_queued(c.acquire_semaphore(1, "sem", 2, 1, 5_000, 5_000).await);

    let e = expect_error_code(
        a.acquire_semaphore(2, "sem", 3, 1, 5_000, 0).await,
        ErrorCode::CapacityMismatch,
    );
    assert_eq!(e.existing_capacity, 2);
    expect_error_code(
        a.acquire(3, "sem", Mode::Mutex, 5_000, 0).await,
        ErrorCode::KindMismatch,
    );

    expect_ok_release(a.release("sem", ga.fencing_token, HolderState::Unset).await);
    expect_granted_push(c.expect_push().await);
}

#[tokio::test]
async fn invalid_requests_get_error_replies_not_disconnects() {
    let srv = TestServer::start().await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    // No mode.
    expect_error_code(
        a.call(cf::Body::Acquire(pb::Acquire {
            request_id: 1,
            name: "m".into(),
            ttl_ms: 1000,
            ..Default::default()
        }))
        .await,
        ErrorCode::InvalidArgument,
    );
    // Zero ttl, empty name.
    expect_error_code(
        a.acquire(2, "m", Mode::Mutex, 0, 0).await,
        ErrorCode::InvalidArgument,
    );
    expect_error_code(
        a.acquire(3, "", Mode::Mutex, 1_000, 0).await,
        ErrorCode::InvalidArgument,
    );
    // Clients cannot set ABANDONED, and an unknown lock is simply not held.
    let g = expect_granted(a.acquire(4, "m", Mode::Mutex, 5_000, 0).await);
    expect_error_code(
        a.release("m", g.fencing_token, HolderState::Abandoned)
            .await,
        ErrorCode::InvalidArgument,
    );
    expect_error_code(
        a.release("nope", 1, HolderState::Unset).await,
        ErrorCode::NotHeld,
    );
    // The connection is still perfectly usable.
    expect_ok_release(a.release("m", g.fencing_token, HolderState::Unset).await);
}

#[tokio::test]
async fn cancel_removes_a_waiter_and_releases_a_grant_that_raced_in() {
    let srv = TestServer::start().await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let (mut b, _) = Client::login(srv.addr, "reports").await;
    let ga = expect_granted(a.acquire(1, "m", Mode::Mutex, 5_000, 0).await);

    expect_queued(b.acquire(5, "m", Mode::Mutex, 5_000, 5_000).await);
    assert_eq!(b.cancel(5).await, pb::cancel_ok::Outcome::Dequeued);
    assert_eq!(b.cancel(5).await, pb::cancel_ok::Outcome::NotFound);
    expect_ok_release(a.release("m", ga.fencing_token, HolderState::Unset).await);
    b.expect_no_push(ms(150)).await;

    // Granted, then cancelled (as a client whose acquire future was dropped would do): lock is freed.
    expect_granted(b.acquire(6, "m", Mode::Mutex, 5_000, 0).await);
    assert_eq!(b.cancel(6).await, pb::cancel_ok::Outcome::ReleasedGrant);
    expect_granted(a.acquire(2, "m", Mode::Mutex, 5_000, 0).await);
}

// ------------------------------------------------------------------------ pools, reconnect, reclaim

#[tokio::test]
async fn any_connection_of_a_session_can_carry_any_request_and_receive_pushes() {
    let srv = TestServer::start().await;
    let (mut holder, _) = Client::login(srv.addr, "reports").await;
    let g = expect_granted(holder.acquire(1, "m", Mode::Mutex, 5_000, 0).await);

    let (mut c1, hello) = Client::login(srv.addr, "billing").await;
    let (mut c2, attached) = Client::attach(srv.addr, &hello).await;
    assert!(attached.view.expect("view").held.is_empty());

    expect_queued(c1.acquire(7, "m", Mode::Mutex, 5_000, 5_000).await);
    expect_ok_release(
        holder
            .release("m", g.fencing_token, HolderState::Unset)
            .await,
    );

    // Pushes may go to any connection of the session, so accept either, but exactly one.
    let (granted, other_got_something) = tokio::select! {
        body = c1.next_push(WAIT) => (body, c2.next_push(ms(200)).await.is_some()),
        body = c2.next_push(WAIT) => (body, c1.next_push(ms(200)).await.is_some()),
    };
    assert!(
        !other_got_something,
        "a push must be delivered once, not broadcast"
    );
    let grant = expect_granted_push(granted.expect("grant push"));
    assert_eq!(grant.request_id, 7);

    // Acquired on c1, released over c2.
    expect_ok_release(
        c2.release("m", grant.fencing_token, HolderState::Stored)
            .await,
    );
}

#[tokio::test]
async fn locks_survive_a_dropped_connection_and_reclaim_shows_them() {
    let srv = TestServer::start().await;
    let (mut other, _) = Client::login(srv.addr, "reports").await;
    expect_granted(other.acquire(1, "busy", Mode::Mutex, 10_000, 0).await);

    let (mut c, hello) = Client::login(srv.addr, "billing").await;
    let g = expect_granted(c.acquire(1, "m", Mode::Mutex, 10_000, 0).await);
    // A second request of this session, still waiting on a lock someone else holds.
    expect_queued(c.acquire(2, "busy", Mode::Mutex, 10_000, 10_000).await);
    drop(c);
    tokio::time::sleep(ms(100)).await;

    let (mut c2, attached) = Client::attach(srv.addr, &hello).await;
    let view = attached.view.expect("view");
    assert_eq!(view.held.len(), 1);
    let held = &view.held[0];
    assert_eq!((held.name.as_str(), held.request_id), ("m", 1));
    assert_eq!(held.fencing_token, g.fencing_token);
    assert_eq!(held.mode, Mode::Mutex as i32);
    assert!(held.ttl_remaining_ms > 0 && held.ttl_remaining_ms <= 10_000);
    assert_eq!(view.pending.len(), 1);
    assert_eq!(
        (view.pending[0].name.as_str(), view.pending[0].request_id),
        ("busy", 2)
    );

    // Retrying the very same request after reconnecting is idempotent: same grant, not a second lock.
    let again = expect_granted(c2.acquire(1, "m", Mode::Mutex, 10_000, 0).await);
    assert_eq!(again.fencing_token, g.fencing_token);
    expect_ok_release(c2.release("m", g.fencing_token, HolderState::Unset).await);
}

#[tokio::test]
async fn attach_requires_the_session_token_and_a_live_session() {
    let srv = TestServer::start().await;
    let (_c, hello) = Client::login(srv.addr, "billing").await;

    let mut bad = Client::connect(srv.addr).await;
    expect_error_code(
        bad.call(attach_body(hello.session_id, &[0u8; 32])).await,
        ErrorCode::Unauthenticated,
    );
    bad.expect_closed().await;

    let mut unknown = Client::connect(srv.addr).await;
    expect_error_code(
        unknown.call(attach_body(12345, &hello.session_token)).await,
        ErrorCode::SessionNotFound,
    );
    unknown.expect_closed().await;
}

#[tokio::test]
async fn a_session_has_a_connection_limit() {
    let srv = TestServer::start_with(|c| c.max_connections_per_session = 2).await;
    let (_c1, hello) = Client::login(srv.addr, "billing").await;
    let (_c2, _) = Client::attach(srv.addr, &hello).await;
    let mut c3 = Client::connect(srv.addr).await;
    expect_error_code(
        c3.call(attach_body(hello.session_id, &hello.session_token))
            .await,
        ErrorCode::ResourceExhausted,
    );
    c3.expect_closed().await;
}

#[tokio::test]
async fn an_idle_session_without_locks_is_forgotten_but_one_holding_a_lock_is_not() {
    let srv = TestServer::start_with(|c| c.session_idle_timeout = ms(300)).await;
    let (empty, hello_empty) = Client::login(srv.addr, "billing").await;
    let (mut holder, hello_holder) = Client::login(srv.addr, "reports").await;
    expect_granted(holder.acquire(1, "m", Mode::Mutex, 30_000, 0).await);
    drop(empty);
    drop(holder);

    tokio::time::sleep(ms(1_600)).await; // idle timeout + a housekeeping tick (500ms), with margin

    let mut c = Client::connect(srv.addr).await;
    expect_error_code(
        c.call(attach_body(
            hello_empty.session_id,
            &hello_empty.session_token,
        ))
        .await,
        ErrorCode::SessionNotFound,
    );
    let (_c, attached) = Client::attach(srv.addr, &hello_holder).await;
    assert_eq!(
        attached.view.expect("view").held.len(),
        1,
        "a session that holds a lock must survive"
    );
}

// ------------------------------------------------------------------------ leases

#[tokio::test]
async fn an_expired_lease_notifies_the_holder_and_hands_over_after_the_lock_delay() {
    let srv = TestServer::start_with(|c| c.lock.lock_delay = ms(150)).await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let (mut b, _) = Client::login(srv.addr, "reports").await;

    let t0 = Instant::now();
    let ga = expect_granted(a.acquire(1, "m", Mode::Mutex, 300, 0).await);
    expect_queued(b.acquire(1, "m", Mode::Mutex, 5_000, 5_000).await);

    match a.expect_push().await {
        sf::Body::LeaseExpired(l) => {
            assert_eq!((l.name.as_str(), l.fencing_token), ("m", ga.fencing_token))
        }
        other => panic!("expected LeaseExpired, got {other:?}"),
    }
    assert!(t0.elapsed() >= ms(300), "expired before its ttl");

    let gb = expect_granted_push(b.expect_push().await);
    assert!(
        t0.elapsed() >= ms(450),
        "handed over before the lock-delay was over: {:?}",
        t0.elapsed()
    );
    assert!(gb.fencing_token > ga.fencing_token);
    let previous = gb.previous.expect("tag");
    assert_eq!(previous.state, HolderState::Abandoned as i32);
    assert_eq!(previous.set_by_token, ga.fencing_token);

    // The zombie holder can no longer release or extend.
    expect_error_code(
        a.release("m", ga.fencing_token, HolderState::Succeeded)
            .await,
        ErrorCode::NotHeld,
    );
    assert_eq!(
        a.extend("m", ga.fencing_token, 1_000).await.code,
        ErrorCode::NotHeld as i32
    );
}

#[tokio::test]
async fn extend_keeps_a_lease_alive_until_the_client_stops() {
    let srv = TestServer::start().await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let (mut b, _) = Client::login(srv.addr, "reports").await;
    let g = expect_granted(a.acquire(1, "m", Mode::Mutex, 300, 0).await);

    for _ in 0..5 {
        tokio::time::sleep(ms(150)).await; // 750ms in total: more than twice the original ttl
        let r = a.extend("m", g.fencing_token, 300).await;
        assert_eq!(r.code, ErrorCode::Ok as i32);
        assert!(r.ttl_remaining_ms > 0 && r.ttl_remaining_ms <= 300);
    }
    expect_error_code(
        b.acquire(1, "m", Mode::Mutex, 1_000, 0).await,
        ErrorCode::Busy,
    );

    match a.expect_push().await {
        sf::Body::LeaseExpired(_) => {} // the client stopped extending
        other => panic!("expected LeaseExpired, got {other:?}"),
    }
    expect_granted(b.acquire(2, "m", Mode::Mutex, 1_000, 0).await);
}

#[tokio::test]
async fn max_hold_caps_the_total_life_of_a_lease_however_often_it_is_extended() {
    let srv = TestServer::start_with(|c| c.lock.max_hold = Some(ms(500))).await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    let t0 = Instant::now();
    let g = expect_granted(a.acquire(1, "m", Mode::Mutex, 300, 0).await);
    let mut expired_at = None;
    while t0.elapsed() < ms(2_000) {
        tokio::time::sleep(ms(100)).await;
        let r = a.extend("m", g.fencing_token, 300).await;
        if r.code != ErrorCode::Ok as i32 {
            expired_at = Some(t0.elapsed());
            break;
        }
    }
    let at = expired_at.expect("lease must not outlive max_hold");
    assert!(at >= ms(500) && at < ms(1_200), "expired at {at:?}");
}

#[tokio::test]
async fn acquires_are_refused_during_the_startup_grace_period() {
    let srv = TestServer::start_with(|c| c.lock.startup_grace = ms(400)).await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;
    expect_error_code(
        a.acquire(1, "m", Mode::Mutex, 1_000, 0).await,
        ErrorCode::Unavailable,
    );
    tokio::time::sleep(ms(450)).await;
    expect_granted(a.acquire(2, "m", Mode::Mutex, 1_000, 0).await);
}

// ------------------------------------------------------------------------ session end and shutdown

#[tokio::test]
async fn close_session_releases_everything_and_closes_every_connection() {
    // A long lock-delay proves a graceful close does not trigger it.
    let srv = TestServer::start_with(|c| c.lock.lock_delay = Duration::from_secs(30)).await;
    let (mut a1, hello) = Client::login(srv.addr, "billing").await;
    let (mut a2, _) = Client::attach(srv.addr, &hello).await;
    let (mut b, _) = Client::login(srv.addr, "reports").await;

    let ga = expect_granted(a1.acquire(1, "m", Mode::Mutex, 10_000, 0).await);
    expect_queued(b.acquire(1, "m", Mode::Mutex, 10_000, 10_000).await);

    match a1.call(cf::Body::CloseSession(pb::CloseSession {})).await {
        sf::Body::CloseSessionOk(_) => {}
        other => panic!("expected CloseSessionOk, got {other:?}"),
    }
    a1.expect_closed().await;
    a2.expect_closed().await;

    let gb = expect_granted_push(b.expect_push().await);
    assert_eq!(
        gb.previous.expect("tag").state,
        HolderState::Abandoned as i32
    );
    assert_eq!(gb.previous.expect("tag").set_by_token, ga.fencing_token);

    let mut late = Client::connect(srv.addr).await;
    expect_error_code(
        late.call(attach_body(hello.session_id, &hello.session_token))
            .await,
        ErrorCode::SessionNotFound,
    );
}

#[tokio::test]
async fn shutdown_notifies_clients_then_closes_their_connections() {
    let srv = TestServer::start().await;
    let (mut c, _) = Client::login(srv.addr, "billing").await;
    srv.shutdown();
    match c.expect_push().await {
        sf::Body::Shutdown(s) => assert!(s.drain_ms > 0),
        other => panic!("expected Shutdown, got {other:?}"),
    }
    c.expect_closed().await;
    srv.stop().await;
}
