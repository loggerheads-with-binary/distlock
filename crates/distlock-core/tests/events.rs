//! Events: named broadcasts with a limited memory (`event_wait` / `event_set`).
//!
//! Every operation goes through `H`, which runs the table's full invariant check afterwards, so
//! these double as invariant tests. Times are milliseconds on the table's clock.

use std::sync::Arc;
use std::time::Duration;

use distlock_core::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}
fn s(n: u64) -> SessionId {
    SessionId(n)
}
fn r(n: u64) -> RequestId {
    RequestId(n)
}

fn cfg() -> Config {
    Config {
        lock_delay: Duration::ZERO,
        event_default_publish: ms(10_000),
        event_max_publish: ms(60_000),
        ..Config::default()
    }
}

struct H {
    t: LockTable,
}

impl H {
    fn new(cfg: Config) -> Self {
        Self {
            t: LockTable::new(cfg),
        }
    }

    fn check(&self) {
        self.t
            .check_invariants()
            .unwrap_or_else(|e| panic!("invariant violated: {e}"));
    }

    fn wait(
        &mut self,
        at: u64,
        session: u64,
        request: u64,
        name: &str,
        wait_ms: u64,
        after: u64,
    ) -> Result<EventWaitStatus, Error> {
        let res = self.t.event_wait(
            ms(at),
            EventWaitRequest {
                session: s(session),
                request: r(request),
                name: name.into(),
                wait: ms(wait_ms),
                after,
            },
        );
        self.check();
        res
    }

    fn set(
        &mut self,
        at: u64,
        name: &str,
        value: Option<&str>,
        publish_ms: Option<u64>,
    ) -> Result<SetOutcome, Error> {
        let res = self.t.event_set(
            ms(at),
            EventSetRequest {
                name: name.into(),
                value: value.map(Arc::from),
                publish_for: publish_ms.map(ms),
            },
        );
        self.check();
        res
    }

    fn cancel(&mut self, at: u64, session: u64, request: u64) -> CancelOutcome {
        let res = self.t.cancel(ms(at), s(session), r(request));
        self.check();
        res
    }

    fn close(&mut self, at: u64, session: u64) {
        self.t.close_session(ms(at), s(session));
        self.check();
    }

    fn view(&mut self, at: u64, session: u64) -> SessionView {
        let v = self.t.session_view(ms(at), s(session));
        self.check();
        v
    }

    fn poll(&mut self, at: u64) -> Vec<Event> {
        self.t.poll(ms(at));
        self.check();
        self.t.take_events()
    }

    fn events(&mut self) -> Vec<Event> {
        self.t.take_events()
    }
}

/// `(session, request, publication)` of every `Published` event, in the order delivered.
fn published(events: &[Event]) -> Vec<(u64, u64, Publication)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Published {
                session,
                request,
                publication,
                ..
            } => Some((session.0, request.0, publication.clone())),
            _ => None,
        })
        .collect()
}

fn timed_out(events: &[Event]) -> Vec<(u64, u64)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::WaitTimedOut {
                session, request, ..
            } => Some((session.0, request.0)),
            _ => None,
        })
        .collect()
}

fn immediate(res: Result<EventWaitStatus, Error>) -> Publication {
    match res {
        Ok(EventWaitStatus::Published(p)) => p,
        other => panic!("expected an immediate publication, got {other:?}"),
    }
}

fn waiting(res: Result<EventWaitStatus, Error>) {
    assert_eq!(res, Ok(EventWaitStatus::Waiting));
}

fn text(p: &Publication) -> Option<&str> {
    p.value.as_deref()
}

// ------------------------------------------------------------------------------------ waking

#[test]
fn a_set_wakes_every_waiter_in_arrival_order_and_counts_them() {
    let mut h = H::new(cfg());
    waiting(h.wait(0, 1, 10, "deploy", 5_000, 0));
    waiting(h.wait(1, 2, 20, "deploy", 5_000, 0));
    waiting(h.wait(2, 3, 30, "deploy", 5_000, 0));
    waiting(h.wait(3, 3, 31, "other", 5_000, 0));
    assert!(h.events().is_empty(), "waiting alone produces no events");

    let out = h.set(10, "deploy", Some("v1.2"), None).unwrap();
    assert_eq!(out.woken, 3, "only the waiters on this name");

    let woken = published(&h.events());
    let who: Vec<(u64, u64)> = woken.iter().map(|(s, r, _)| (*s, *r)).collect();
    assert_eq!(who, vec![(1, 10), (2, 20), (3, 30)], "arrival order");
    for (_, _, p) in &woken {
        assert_eq!(p.id, out.id, "one publication, one id");
        assert_eq!(text(p), Some("v1.2"));
        assert_eq!(p.age, Duration::ZERO, "woken by the set itself");
    }

    // The other event's waiter is untouched.
    assert_eq!(h.view(11, 3).pending_events.len(), 1);
    assert!(h.view(11, 1).pending_events.is_empty());
}

#[test]
fn setting_an_event_nobody_waits_for_wakes_nobody() {
    let mut h = H::new(cfg());
    let out = h.set(0, "nobody", None, None).unwrap();
    assert_eq!(out.woken, 0);
    assert!(h.events().is_empty());
}

#[test]
fn a_set_without_a_value_publishes_no_value_and_an_empty_value_is_different() {
    let mut h = H::new(cfg());
    waiting(h.wait(0, 1, 1, "e", 5_000, 0));
    waiting(h.wait(0, 2, 2, "e", 5_000, 0));
    h.set(1, "e", None, Some(0)).unwrap();
    assert_eq!(text(&published(&h.events())[0].2), None);

    waiting(h.wait(2, 1, 3, "e", 5_000, 0));
    h.set(3, "e", Some(""), Some(0)).unwrap();
    assert_eq!(text(&published(&h.events())[0].2), Some(""));
}

// ------------------------------------------------------------------------------ publish window

#[test]
fn a_wait_inside_the_publish_window_returns_at_once_with_the_value_and_its_age() {
    let mut h = H::new(cfg());
    let out = h.set(1_000, "e", Some("payload"), Some(5_000)).unwrap();
    assert_eq!(out.publish_for, ms(5_000));

    let p = immediate(h.wait(3_500, 1, 1, "e", 1_000, 0));
    assert_eq!(p.id, out.id);
    assert_eq!(text(&p), Some("payload"));
    assert_eq!(p.age, ms(2_500));
    assert!(
        h.view(3_500, 1).pending_events.is_empty(),
        "nothing registered"
    );

    // Every late joiner in the window gets it, not just the first.
    let again = immediate(h.wait(5_999, 2, 2, "e", 1_000, 0));
    assert_eq!(again.id, out.id);
}

#[test]
fn after_the_window_the_event_is_gone_and_waiters_wait_for_the_next_set() {
    let mut h = H::new(cfg());
    let first = h.set(0, "e", Some("old"), Some(1_000)).unwrap();
    assert_eq!(h.t.stats().events, 1, "kept while published");

    // Exactly at the end of the window it is already over.
    waiting(h.wait(1_000, 1, 1, "e", 60_000, 0));
    assert!(h.events().is_empty());

    let second = h.set(5_000, "e", Some("new"), Some(1_000)).unwrap();
    assert_eq!(second.woken, 1);
    let woken = published(&h.events());
    assert_eq!(text(&woken[0].2), Some("new"), "not the expired value");
    assert!(second.id > first.id);

    h.poll(10_000);
    assert_eq!(
        h.t.stats().events,
        0,
        "the name is collected once the window ends"
    );
}

#[test]
fn a_zero_window_wakes_only_the_waiters_present_now() {
    let mut h = H::new(cfg());
    waiting(h.wait(0, 1, 1, "e", 5_000, 0));
    let out = h.set(1, "e", Some("blink"), Some(0)).unwrap();
    assert_eq!((out.woken, out.publish_for), (1, Duration::ZERO));
    h.events();
    assert_eq!(h.t.stats().events, 0, "nothing left to remember");

    waiting(h.wait(2, 2, 2, "e", 5_000, 0));
}

#[test]
fn the_default_window_applies_when_none_is_given_and_the_maximum_caps_it() {
    let mut h = H::new(cfg());
    assert_eq!(h.set(0, "a", None, None).unwrap().publish_for, ms(10_000));
    assert_eq!(
        h.set(0, "b", None, Some(10_000_000)).unwrap().publish_for,
        ms(60_000)
    );
    immediate(h.wait(9_999, 1, 1, "a", 1_000, 0));
    waiting(h.wait(10_000, 1, 2, "a", 1_000, 0));
}

#[test]
fn the_latest_set_replaces_the_earlier_one_even_with_a_zero_window() {
    let mut h = H::new(cfg());
    h.set(0, "e", Some("one"), Some(10_000)).unwrap();
    let two = h.set(1, "e", Some("two"), Some(10_000)).unwrap();
    let p = immediate(h.wait(2, 1, 1, "e", 1_000, 0));
    assert_eq!((p.id, text(&p)), (two.id, Some("two")));

    // A zero-window set withdraws what was published: a wait now has to wait.
    h.set(3, "e", Some("three"), Some(0)).unwrap();
    waiting(h.wait(4, 2, 2, "e", 1_000, 0));
}

#[test]
fn a_longer_second_window_outlives_the_first_timer() {
    // The first set arms an expiry timer at 1s; the second extends the window to 9s. The first
    // timer must not end the publication early.
    let mut h = H::new(cfg());
    h.set(0, "e", Some("a"), Some(1_000)).unwrap();
    let b = h.set(500, "e", Some("b"), Some(8_500)).unwrap();
    h.poll(1_000);
    assert_eq!(h.t.stats().events, 1);
    let p = immediate(h.wait(5_000, 1, 1, "e", 1_000, 0));
    assert_eq!(p.id, b.id);
    h.poll(9_000);
    assert_eq!(h.t.stats().events, 0);
}

// ----------------------------------------------------------------------------------------- after

#[test]
fn after_skips_a_publication_the_caller_has_already_handled() {
    let mut h = H::new(cfg());
    let first = h.set(0, "e", Some("1"), Some(10_000)).unwrap();

    // Asked for something newer than `first`: the live publication does not count.
    waiting(h.wait(1, 1, 1, "e", 60_000, first.id));
    // Asked for anything newer than before it existed: it counts.
    assert_eq!(
        immediate(h.wait(1, 2, 2, "e", 1_000, first.id - 1)).id,
        first.id
    );

    let second = h.set(2, "e", Some("2"), Some(10_000)).unwrap();
    assert_eq!(second.woken, 1, "the waiter that asked for newer");
    assert_eq!(published(&h.events())[0].2.id, second.id);
    assert!(second.id > first.id);
}

// -------------------------------------------------------------------------------------- timeouts

#[test]
fn a_wait_times_out_exactly_once_and_is_then_forgotten() {
    let mut h = H::new(cfg());
    waiting(h.wait(0, 1, 7, "e", 2_000, 0));
    assert!(h.poll(1_999).is_empty());
    assert_eq!(timed_out(&h.poll(2_000)), vec![(1, 7)]);
    assert!(h.view(2_000, 1).pending_events.is_empty());
    assert_eq!(
        h.t.stats(),
        Stats {
            entries: 0,
            events: 0,
            sessions: 0,
            timers: 0
        }
    );

    // A set afterwards wakes nobody.
    assert_eq!(h.set(2_001, "e", None, Some(0)).unwrap().woken, 0);
}

#[test]
fn a_zero_wait_is_a_poll_that_reports_busy_unless_something_is_published() {
    let mut h = H::new(cfg());
    assert_eq!(h.wait(0, 1, 1, "e", 0, 0), Err(Error::Busy));
    assert_eq!(h.t.stats().events, 0, "a failed poll leaves nothing behind");

    h.set(1, "e", Some("x"), Some(1_000)).unwrap();
    assert_eq!(text(&immediate(h.wait(2, 1, 2, "e", 0, 0))), Some("x"));
    assert_eq!(h.wait(2_000, 1, 3, "e", 0, 0), Err(Error::Busy));
}

#[test]
fn a_wait_is_capped_by_the_servers_max_wait() {
    let mut h = H::new(Config {
        max_wait: ms(1_000),
        ..cfg()
    });
    waiting(h.wait(0, 1, 1, "e", 3_600_000, 0));
    assert_eq!(timed_out(&h.poll(1_000)), vec![(1, 1)]);
}

#[test]
fn a_reused_request_id_is_not_timed_out_by_its_predecessors_timer() {
    let mut h = H::new(cfg());
    waiting(h.wait(0, 1, 1, "e", 1_000, 0));
    assert_eq!(h.cancel(100, 1, 1), CancelOutcome::Dequeued);
    // Same request id, longer patience: the first wait's timer (at 1s) is still in the table.
    waiting(h.wait(200, 1, 1, "e", 5_000, 0));
    assert!(
        timed_out(&h.poll(1_000)).is_empty(),
        "the stale timer must not end the new wait"
    );
    assert_eq!(timed_out(&h.poll(5_200)), vec![(1, 1)]);
}

#[test]
fn a_woken_waiters_old_timeout_timer_does_nothing_later() {
    let mut h = H::new(cfg());
    waiting(h.wait(0, 1, 1, "e", 2_000, 0));
    h.set(10, "e", None, Some(0)).unwrap();
    h.events();
    assert!(
        timed_out(&h.poll(5_000)).is_empty(),
        "already woken; no late timeout"
    );
}

// ------------------------------------------------------------------------------------- cancelling

#[test]
fn cancel_removes_a_waiter_so_a_later_set_does_not_wake_it() {
    let mut h = H::new(cfg());
    waiting(h.wait(0, 1, 1, "e", 5_000, 0));
    waiting(h.wait(0, 2, 2, "e", 5_000, 0));
    assert_eq!(h.cancel(1, 1, 1), CancelOutcome::Dequeued);
    assert_eq!(h.cancel(1, 1, 1), CancelOutcome::NotFound, "only once");
    assert_eq!(h.cancel(1, 9, 9), CancelOutcome::NotFound);

    let out = h.set(2, "e", None, Some(0)).unwrap();
    assert_eq!(out.woken, 1);
    assert_eq!(published(&h.events())[0].0, 2);
}

#[test]
fn cancelling_the_last_waiter_collects_the_name() {
    let mut h = H::new(cfg());
    waiting(h.wait(0, 1, 1, "e", 5_000, 0));
    assert_eq!(h.t.stats().events, 1);
    h.cancel(1, 1, 1);
    assert_eq!(h.t.stats().events, 0);
}

// -------------------------------------------------------------------------------------- sessions

#[test]
fn closing_or_revoking_a_session_drops_its_waits_but_not_other_sessions() {
    for forced in [false, true] {
        let mut h = H::new(cfg());
        waiting(h.wait(0, 1, 1, "e", 5_000, 0));
        waiting(h.wait(0, 1, 2, "f", 5_000, 0));
        waiting(h.wait(0, 2, 3, "e", 5_000, 0));
        if forced {
            h.t.revoke_session(ms(1), s(1));
            h.check();
        } else {
            h.close(1, 1);
        }
        assert!(h.view(1, 1).pending_events.is_empty());
        assert_eq!(
            h.t.stats().events,
            1,
            "only session 2's wait on `e` remains"
        );

        let out = h.set(2, "e", None, Some(0)).unwrap();
        assert_eq!(out.woken, 1);
        assert_eq!(published(&h.events())[0].0, 2);
    }
}

#[test]
fn the_session_view_lists_pending_event_waits_for_reclaim() {
    let mut h = H::new(cfg());
    waiting(h.wait(0, 1, 5, "e", 5_000, 0));
    waiting(h.wait(0, 1, 3, "f", 5_000, 0));
    let view = h.view(1, 1);
    let pending: Vec<(u64, &str)> = view
        .pending_events
        .iter()
        .map(|p| (p.request.0, &*p.name))
        .collect();
    assert_eq!(pending, vec![(3, "f"), (5, "e")], "sorted by request id");
    assert!(view.held.is_empty() && view.pending.is_empty());
    assert!(
        h.t.session_has_requests(s(1)),
        "an event wait keeps the session alive"
    );
}

#[test]
fn a_session_that_only_waits_on_events_is_forgotten_when_the_wait_ends() {
    let mut h = H::new(cfg());
    waiting(h.wait(0, 1, 1, "e", 1_000, 0));
    assert!(h.t.session_has_requests(s(1)));
    h.poll(1_000);
    assert!(!h.t.session_has_requests(s(1)));
}

// ----------------------------------------------------------------------------- ids and retrying

#[test]
fn publication_ids_share_the_fencing_token_counter() {
    let mut h = H::new(cfg());
    let grant = |h: &mut H, request: u64, name: &str| {
        let res =
            h.t.acquire(
                ms(0),
                AcquireRequest {
                    session: s(1),
                    request: r(request),
                    name: name.into(),
                    want: Want::Mutex,
                    ttl: ms(1_000),
                    wait: ms(0),
                },
            )
            .unwrap();
        h.check();
        match res {
            AcquireStatus::Granted(g) => g.token.0,
            other => panic!("expected a grant, got {other:?}"),
        }
    };
    let t1 = grant(&mut h, 1, "a");
    let id1 = h.set(0, "e", None, Some(0)).unwrap().id;
    let t2 = grant(&mut h, 2, "b");
    let id2 = h.set(0, "e", None, Some(0)).unwrap().id;
    assert!(t1 < id1 && id1 < t2 && t2 < id2, "{t1} {id1} {t2} {id2}");
}

#[test]
fn retrying_a_wait_with_the_same_request_id_is_idempotent() {
    let mut h = H::new(cfg());
    waiting(h.wait(0, 1, 1, "e", 5_000, 0));
    waiting(h.wait(1, 1, 1, "e", 5_000, 0));
    assert_eq!(h.view(1, 1).pending_events.len(), 1, "still one wait");
    assert_eq!(h.set(2, "e", None, Some(0)).unwrap().woken, 1, "woken once");

    waiting(h.wait(3, 1, 2, "e", 5_000, 0));
    assert_eq!(
        h.wait(3, 1, 2, "different", 5_000, 0),
        Err(Error::RequestConflict)
    );
}

#[test]
fn a_request_id_cannot_be_both_a_lock_request_and_an_event_wait() {
    let mut h = H::new(cfg());
    waiting(h.wait(0, 1, 1, "e", 5_000, 0));
    let acquire = |request: u64| AcquireRequest {
        session: s(1),
        request: r(request),
        name: "m".into(),
        want: Want::Mutex,
        ttl: ms(1_000),
        wait: ms(0),
    };
    assert_eq!(h.t.acquire(ms(1), acquire(1)), Err(Error::RequestConflict));
    h.check();

    h.t.acquire(ms(1), acquire(2)).unwrap();
    assert_eq!(h.wait(2, 1, 2, "e", 5_000, 0), Err(Error::RequestConflict));
}

// ----------------------------------------------------------------------------------------- limits

#[test]
fn names_and_values_are_validated() {
    let mut h = H::new(Config {
        max_name_len: 8,
        max_event_value_len: 4,
        ..cfg()
    });
    assert!(matches!(
        h.wait(0, 1, 1, "", 1_000, 0),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        h.wait(0, 1, 1, "much-too-long", 1_000, 0),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        h.set(0, "", None, None),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        h.set(0, "e", Some("12345"), None),
        Err(Error::InvalidArgument(_))
    ));
    assert!(
        h.set(0, "e", Some("1234"), None).is_ok(),
        "exactly the limit is fine"
    );
    assert_eq!(h.t.stats().sessions, 0);
}

#[test]
fn the_wait_queue_of_one_event_is_bounded() {
    let mut h = H::new(Config {
        max_waiters_per_lock: 2,
        ..cfg()
    });
    waiting(h.wait(0, 1, 1, "e", 5_000, 0));
    waiting(h.wait(0, 2, 2, "e", 5_000, 0));
    assert!(matches!(
        h.wait(0, 3, 3, "e", 5_000, 0),
        Err(Error::ResourceExhausted(_))
    ));
    waiting(h.wait(0, 3, 4, "other", 5_000, 0));
}

#[test]
fn event_waits_count_against_the_sessions_request_limit() {
    let mut h = H::new(Config {
        max_requests_per_session: 2,
        ..cfg()
    });
    waiting(h.wait(0, 1, 1, "a", 5_000, 0));
    waiting(h.wait(0, 1, 2, "b", 5_000, 0));
    assert!(matches!(
        h.wait(0, 1, 3, "c", 5_000, 0),
        Err(Error::ResourceExhausted(_))
    ));
    // ... and so do lock requests.
    let res = h.t.acquire(
        ms(0),
        AcquireRequest {
            session: s(1),
            request: r(4),
            name: "m".into(),
            want: Want::Mutex,
            ttl: ms(1_000),
            wait: ms(0),
        },
    );
    assert!(matches!(res, Err(Error::ResourceExhausted(_))));
}

#[test]
fn many_waiters_and_many_sets_drain_to_nothing() {
    let mut h = H::new(cfg());
    for i in 0..200u64 {
        waiting(h.wait(i, i % 7 + 1, i + 1, &format!("e{}", i % 5), 1_000 + i, 0));
    }
    for i in 0..50u64 {
        h.set(300 + i, &format!("e{}", i % 5), Some("v"), Some(100))
            .unwrap();
    }
    h.events();
    h.poll(1_000_000);
    assert_eq!(
        h.t.stats(),
        Stats {
            entries: 0,
            events: 0,
            sessions: 0,
            timers: 0
        }
    );
}
