//! One test per semantic promised in DESIGN.md. Every operation goes through `H`, which runs the
//! table's full invariant check afterwards, so these double as invariant tests.

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
fn sem(capacity: u32, permits: u32) -> Want {
    Want::Semaphore { capacity, permits }
}

/// Defaults, but with no lock-delay so tests only see it when they ask for it.
fn cfg() -> Config {
    Config {
        lock_delay: Duration::ZERO,
        ..Config::default()
    }
}

fn req(
    session: u64,
    request: u64,
    name: &str,
    want: Want,
    ttl_ms: u64,
    wait_ms: u64,
) -> AcquireRequest {
    AcquireRequest {
        session: s(session),
        request: r(request),
        name: name.into(),
        want,
        ttl: ms(ttl_ms),
        wait: ms(wait_ms),
    }
}

fn granted(res: Result<AcquireStatus, Error>) -> GrantInfo {
    match res {
        Ok(AcquireStatus::Granted(g)) => g,
        other => panic!("expected an immediate grant, got {other:?}"),
    }
}

fn queued(res: Result<AcquireStatus, Error>) {
    assert_eq!(res, Ok(AcquireStatus::Queued));
}

/// All `Granted` events as (session, request, grant).
fn grants(events: &[Event]) -> Vec<(SessionId, RequestId, GrantInfo)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Granted {
                session,
                request,
                grant,
                ..
            } => Some((*session, *request, *grant)),
            _ => None,
        })
        .collect()
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
    fn acquire(&mut self, at: u64, req: AcquireRequest) -> Result<AcquireStatus, Error> {
        let res = self.t.acquire(ms(at), req);
        self.check();
        res
    }
    fn release(
        &mut self,
        at: u64,
        session: u64,
        name: &str,
        token: FencingToken,
        state: Option<ReleaseState>,
    ) -> Result<(), Error> {
        let res = self.t.release(ms(at), s(session), name, token, state, None);
        self.check();
        res
    }
    fn extend(
        &mut self,
        at: u64,
        session: u64,
        name: &str,
        token: FencingToken,
        ttl_ms: u64,
    ) -> Result<Duration, Error> {
        let res = self.t.extend(ms(at), s(session), name, token, ms(ttl_ms));
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
    /// Advance time and return everything that happened (including earlier undrained events).
    fn poll(&mut self, at: u64) -> Vec<Event> {
        self.t.poll(ms(at));
        self.check();
        self.t.take_events()
    }
    fn events(&mut self) -> Vec<Event> {
        self.t.take_events()
    }
}

// ------------------------------------------------------------------ mutex

#[test]
fn mutex_is_granted_in_fifo_order() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 10_000, 5_000)));
    queued(h.acquire(1, req(2, 1, "m", Want::Mutex, 10_000, 5_000)));
    queued(h.acquire(2, req(3, 1, "m", Want::Mutex, 10_000, 5_000)));
    assert!(h.events().is_empty());

    h.release(3, 1, "m", a.token, None).unwrap();
    let g = grants(&h.events());
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].0, s(2));

    h.release(4, 2, "m", g[0].2.token, None).unwrap();
    let g2 = grants(&h.events());
    assert_eq!(g2.len(), 1);
    assert_eq!(g2[0].0, s(3));
    assert!(g2[0].2.token > g[0].2.token && g[0].2.token > a.token);
}

#[test]
fn try_lock_fails_fast_and_leaves_no_residue() {
    let mut h = H::new(cfg());
    granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 1_000, 0)));
    assert_eq!(
        h.acquire(1, req(2, 1, "m", Want::Mutex, 1_000, 0)),
        Err(Error::Busy)
    );
    assert_eq!(h.t.stats().sessions, 1);
}

#[test]
fn queued_acquire_times_out() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 10_000, 0)));
    queued(h.acquire(0, req(2, 7, "m", Want::Mutex, 10_000, 100)));
    assert!(h.poll(99).is_empty());
    assert_eq!(
        h.poll(100),
        vec![Event::WaitTimedOut {
            session: s(2),
            request: r(7),
            name: "m".into()
        }]
    );
    h.release(101, 1, "m", a.token, None).unwrap();
    assert!(
        h.events().is_empty(),
        "a timed-out waiter must never be granted"
    );
    assert_eq!(h.t.stats().entries, 0);
}

#[test]
fn wait_is_capped_by_max_wait() {
    let mut h = H::new(Config {
        max_wait: ms(50),
        ..cfg()
    });
    granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 10_000, 0)));
    queued(h.acquire(0, req(2, 1, "m", Want::Mutex, 10_000, 99_999)));
    assert_eq!(h.poll(50).len(), 1);
}

#[test]
fn a_stale_wait_timer_does_not_time_out_a_reused_request_id() {
    let mut h = H::new(cfg());
    granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 10_000, 0)));
    queued(h.acquire(0, req(2, 1, "m", Want::Mutex, 10_000, 100))); // timer at t=100
    assert_eq!(h.cancel(10, 2, 1), CancelOutcome::Dequeued); // ...which is now stale
    queued(h.acquire(20, req(2, 1, "m", Want::Mutex, 10_000, 1_000))); // same ids, deadline t=1020
    assert!(
        h.poll(100).is_empty(),
        "the first incarnation's timer must not fire the second"
    );
    assert_eq!(h.poll(1_020).len(), 1);
}

// ------------------------------------------------------------------ leases

#[test]
fn expired_lease_notifies_holder_tags_abandoned_and_applies_lock_delay() {
    let mut h = H::new(Config {
        lock_delay: ms(500),
        ..Config::default()
    });
    let a = granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 1_000, 0)));
    queued(h.acquire(10, req(2, 1, "m", Want::Mutex, 1_000, 10_000)));
    assert!(h.events().is_empty());

    assert_eq!(
        h.poll(1_000),
        vec![Event::LeaseExpired {
            session: s(1),
            name: "m".into(),
            token: a.token
        }]
    );
    assert!(
        h.poll(1_499).is_empty(),
        "nobody is granted during lock-delay"
    );

    let ev = h.poll(1_500);
    let g = grants(&ev);
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].0, s(2));
    assert!(g[0].2.token > a.token);
    assert_eq!(g[0].2.ttl_remaining, ms(1_000));
    assert_eq!(
        g[0].2.previous,
        Some(PreviousState {
            state: HolderState::Abandoned,
            set_by: a.token,
            age: ms(500)
        })
    );
}

#[test]
fn a_long_gap_between_polls_behaves_like_frequent_polling() {
    let mut h = H::new(Config {
        lock_delay: ms(500),
        ..Config::default()
    });
    granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 1_000, 0)));
    queued(h.acquire(10, req(2, 1, "m", Want::Mutex, 1_000, 10_000)));
    // Single poll far past expiry + delay: the grant happens at logical t=1500, not at t=9000.
    let ev = h.poll(9_000);
    let g = grants(&ev);
    assert_eq!(g.len(), 1);
    // lease for s2 started at 1500 and expires at 2500, so by 9000 it has expired too
    assert!(
        ev.iter()
            .any(|e| matches!(e, Event::LeaseExpired { session, .. } if *session == s(2)))
    );
    assert_eq!(g[0].2.previous.unwrap().age, ms(500));
}

#[test]
fn zombie_holder_cannot_release_or_extend_after_regrant() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 1_000, 0)));
    queued(h.acquire(1, req(2, 1, "m", Want::Mutex, 10_000, 10_000)));
    let ev = h.poll(1_000);
    assert!(ev.iter().any(|e| matches!(e, Event::LeaseExpired { .. })));
    let b = grants(&ev)[0].2;

    assert_eq!(
        h.release(1_001, 1, "m", a.token, Some(ReleaseState::Succeeded)),
        Err(Error::NotHeld)
    );
    assert_eq!(h.extend(1_001, 1, "m", a.token, 5_000), Err(Error::NotHeld));
    assert_eq!(
        h.view(1_002, 2).held.len(),
        1,
        "the new holder is unaffected"
    );
    h.release(1_003, 2, "m", b.token, None).unwrap();
}

#[test]
fn extend_pushes_expiry_out_but_never_shortens_it() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 1_000, 0)));
    assert_eq!(h.extend(900, 1, "m", a.token, 1_000), Ok(ms(1_000))); // now expires at 1900
    assert!(h.poll(1_500).is_empty());
    assert_eq!(
        h.extend(1_500, 1, "m", a.token, 100),
        Ok(ms(400)),
        "shorter request keeps the longer lease"
    );
    assert_eq!(h.poll(1_900).len(), 1);
    assert_eq!(
        h.extend(1_901, 1, "m", a.token, 1_000),
        Err(Error::NotHeld),
        "expiry is never undone"
    );
}

#[test]
fn extend_is_capped_per_call_and_in_total() {
    let mut h = H::new(Config {
        max_ttl: ms(2_000),
        max_hold: Some(ms(3_000)),
        ..cfg()
    });
    let a = granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 10_000, 0)));
    assert_eq!(a.ttl_remaining, ms(2_000), "grant ttl clamped to max_ttl");
    assert_eq!(
        h.extend(1_000, 1, "m", a.token, 60_000),
        Ok(ms(2_000)),
        "per-call cap"
    );
    assert_eq!(
        h.extend(2_000, 1, "m", a.token, 60_000),
        Ok(ms(1_000)),
        "max_hold: 3000 total from the grant"
    );
    assert_eq!(h.poll(3_000).len(), 1);
}

#[test]
fn extend_rejects_zero_ttl_and_foreign_sessions() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 1_000, 0)));
    assert!(matches!(
        h.extend(1, 1, "m", a.token, 0),
        Err(Error::InvalidArgument(_))
    ));
    assert_eq!(h.extend(1, 2, "m", a.token, 500), Err(Error::NotHeld));
}

#[test]
fn next_deadline_reports_the_earliest_timer() {
    let mut h = H::new(cfg());
    assert_eq!(h.t.next_deadline(), None);
    granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 1_000, 0)));
    assert_eq!(h.t.next_deadline(), Some(ms(1_000)));
}

// ------------------------------------------------------------------ rwlock

#[test]
fn rwlock_readers_share_and_a_waiting_writer_blocks_new_readers() {
    let mut h = H::new(cfg());
    let r1 = granted(h.acquire(0, req(1, 1, "rw", Want::Read, 10_000, 0)));
    let r2 = granted(h.acquire(0, req(2, 1, "rw", Want::Read, 10_000, 0)));
    queued(h.acquire(1, req(3, 1, "rw", Want::Write, 10_000, 10_000)));
    queued(h.acquire(2, req(4, 1, "rw", Want::Read, 10_000, 10_000)));
    assert_eq!(
        h.acquire(2, req(5, 1, "rw", Want::Read, 10_000, 0)),
        Err(Error::Busy),
        "a reader may not jump a waiting writer"
    );

    h.release(3, 1, "rw", r1.token, None).unwrap();
    assert!(h.events().is_empty());
    h.release(4, 2, "rw", r2.token, None).unwrap();
    let g = grants(&h.events());
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].0, s(3));

    h.release(5, 3, "rw", g[0].2.token, None).unwrap();
    let g = grants(&h.events());
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].0, s(4));
}

#[test]
fn consecutive_readers_at_the_queue_head_are_granted_together() {
    let mut h = H::new(cfg());
    let w = granted(h.acquire(0, req(1, 1, "rw", Want::Write, 10_000, 0)));
    queued(h.acquire(1, req(2, 1, "rw", Want::Read, 10_000, 10_000)));
    queued(h.acquire(1, req(3, 1, "rw", Want::Read, 10_000, 10_000)));
    queued(h.acquire(1, req(4, 1, "rw", Want::Write, 10_000, 10_000)));
    queued(h.acquire(1, req(5, 1, "rw", Want::Read, 10_000, 10_000)));
    h.release(2, 1, "rw", w.token, None).unwrap();
    let who: Vec<_> = grants(&h.events()).iter().map(|g| g.0).collect();
    assert_eq!(
        who,
        vec![s(2), s(3)],
        "readers batch up to the next writer, not past it"
    );
}

#[test]
fn a_timed_out_writer_lets_the_readers_behind_it_in() {
    let mut h = H::new(cfg());
    granted(h.acquire(0, req(1, 1, "rw", Want::Read, 10_000, 0)));
    queued(h.acquire(1, req(2, 1, "rw", Want::Write, 10_000, 99)));
    queued(h.acquire(2, req(3, 1, "rw", Want::Read, 10_000, 10_000)));
    let ev = h.poll(100);
    assert!(matches!(ev[0], Event::WaitTimedOut { session, .. } if session == s(2)));
    assert_eq!(grants(&ev).len(), 1);
    assert_eq!(grants(&ev)[0].0, s(3));
}

// ------------------------------------------------------------------ semaphore

#[test]
fn semaphore_is_fifo_with_head_of_line_blocking() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "s", sem(3, 2), 10_000, 0)));
    queued(h.acquire(1, req(2, 1, "s", sem(3, 2), 10_000, 10_000)));
    // One permit is free and this request would fit, but it must not jump the 2-permit request.
    queued(h.acquire(2, req(3, 1, "s", sem(3, 1), 10_000, 10_000)));

    h.release(3, 1, "s", a.token, None).unwrap();
    let who: Vec<_> = grants(&h.events()).iter().map(|g| g.0).collect();
    assert_eq!(who, vec![s(2), s(3)]);
}

#[test]
fn semaphore_disappears_when_its_last_user_is_gone_and_can_be_redefined() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "s", sem(2, 1), 10_000, 0)));
    assert_eq!(
        h.acquire(1, req(2, 1, "s", sem(4, 1), 10_000, 0)),
        Err(Error::CapacityMismatch { existing: 2 })
    );
    assert_eq!(h.t.stats().entries, 1);
    h.release(2, 1, "s", a.token, None).unwrap();
    assert_eq!(
        h.t.stats().entries,
        0,
        "no residue: a redeployed app may pick a new capacity"
    );
    granted(h.acquire(3, req(2, 2, "s", sem(4, 1), 10_000, 0)));
}

#[test]
fn semaphore_arguments_are_validated() {
    let mut h = H::new(cfg());
    for bad in [sem(0, 1), sem(2, 0), sem(2, 3)] {
        assert!(matches!(
            h.acquire(0, req(1, 1, "s", bad, 1_000, 0)),
            Err(Error::InvalidArgument(_))
        ));
    }
}

#[test]
fn a_name_has_one_kind_while_it_exists() {
    let mut h = H::new(cfg());
    granted(h.acquire(0, req(1, 1, "x", Want::Mutex, 10_000, 0)));
    assert_eq!(
        h.acquire(1, req(2, 1, "x", Want::Read, 10_000, 0)),
        Err(Error::KindMismatch {
            existing: Kind::Mutex
        })
    );
    assert_eq!(
        h.acquire(1, req(2, 2, "x", sem(1, 1), 10_000, 0)),
        Err(Error::KindMismatch {
            existing: Kind::Mutex
        })
    );
}

// ------------------------------------------------------------------ state tags

#[test]
fn state_tag_flows_to_the_next_holder_and_survives_a_silent_release() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "job", Want::Mutex, 5_000, 0)));
    assert_eq!(a.previous, None);
    h.release(1, 1, "job", a.token, Some(ReleaseState::Succeeded))
        .unwrap();

    let b = granted(h.acquire(2, req(2, 1, "job", Want::Mutex, 5_000, 0)));
    assert_eq!(
        b.previous,
        Some(PreviousState {
            state: HolderState::Succeeded,
            set_by: a.token,
            age: ms(1)
        })
    );
    // B sees SUCCEEDED, skips the work, releases without a tag: the fact must not be erased.
    h.release(3, 2, "job", b.token, None).unwrap();
    let c = granted(h.acquire(4, req(3, 1, "job", Want::Mutex, 5_000, 0)));
    assert_eq!(
        c.previous.map(|p| (p.state, p.set_by)),
        Some((HolderState::Succeeded, a.token))
    );

    // An explicit new tag replaces it.
    h.release(5, 3, "job", c.token, Some(ReleaseState::Failed))
        .unwrap();
    let d = granted(h.acquire(6, req(4, 1, "job", Want::Mutex, 5_000, 0)));
    assert_eq!(
        d.previous.map(|p| (p.state, p.set_by)),
        Some((HolderState::Failed, c.token))
    );
}

#[test]
fn state_tag_expires_and_the_name_is_garbage_collected() {
    let mut h = H::new(Config {
        default_state_ttl: ms(1_000),
        ..cfg()
    });
    let a = granted(h.acquire(0, req(1, 1, "job", Want::Mutex, 5_000, 0)));
    h.release(10, 1, "job", a.token, Some(ReleaseState::Stored))
        .unwrap();
    assert_eq!(h.t.stats().entries, 1, "tag keeps the name alive");
    h.poll(1_009);
    assert_eq!(h.t.stats().entries, 1);
    h.poll(1_010);
    assert_eq!(h.t.stats().entries, 0);
    let b = granted(h.acquire(1_011, req(2, 1, "job", Want::Mutex, 5_000, 0)));
    assert_eq!(b.previous, None);
}

#[test]
fn releaser_can_choose_the_tag_ttl_but_not_beyond_the_server_cap() {
    let mut h = H::new(Config {
        default_state_ttl: ms(1_000),
        max_state_ttl: ms(2_000),
        ..cfg()
    });
    let a = granted(h.acquire(0, req(1, 1, "job", Want::Mutex, 5_000, 0)));
    h.t.release(
        ms(0),
        s(1),
        "job",
        a.token,
        Some(ReleaseState::Stored),
        Some(ms(60_000)),
    )
    .unwrap();
    h.poll(1_999);
    assert_eq!(h.t.stats().entries, 1);
    h.poll(2_000);
    assert_eq!(h.t.stats().entries, 0);
}

#[test]
fn readers_see_the_last_writers_tag_but_cannot_set_one() {
    let mut h = H::new(cfg());
    let w = granted(h.acquire(0, req(1, 1, "rw", Want::Write, 5_000, 0)));
    h.release(1, 1, "rw", w.token, Some(ReleaseState::Stored))
        .unwrap();

    let rd = granted(h.acquire(2, req(2, 1, "rw", Want::Read, 5_000, 0)));
    assert_eq!(rd.previous.map(|p| p.state), Some(HolderState::Stored));
    assert!(matches!(
        h.release(3, 2, "rw", rd.token, Some(ReleaseState::Succeeded)),
        Err(Error::InvalidArgument(_))
    ));
    assert_eq!(
        h.view(4, 2).held.len(),
        1,
        "a rejected release must not release anything"
    );
    h.release(5, 2, "rw", rd.token, None).unwrap();
}

#[test]
fn semaphore_holders_cannot_set_a_tag() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "s", sem(2, 1), 5_000, 0)));
    assert!(matches!(
        h.release(1, 1, "s", a.token, Some(ReleaseState::Failed)),
        Err(Error::InvalidArgument(_))
    ));
}

// ------------------------------------------------------------------ idempotency, cancel, sessions

#[test]
fn retrying_a_request_returns_the_same_state_instead_of_acquiring_twice() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 1_000, 0)));
    let again = granted(h.acquire(5, req(1, 1, "m", Want::Mutex, 1_000, 0)));
    assert_eq!(again.token, a.token);
    assert_eq!(again.ttl_remaining, ms(995));
    assert_eq!(
        h.acquire(6, req(1, 1, "other", Want::Mutex, 1_000, 0)),
        Err(Error::RequestConflict)
    );
    assert_eq!(
        h.acquire(6, req(1, 1, "m", Want::Read, 1_000, 0)),
        Err(Error::RequestConflict)
    );

    // A retried *pending* request is not queued twice.
    queued(h.acquire(7, req(2, 1, "m", Want::Mutex, 1_000, 10_000)));
    queued(h.acquire(8, req(2, 1, "m", Want::Mutex, 1_000, 10_000)));
    h.release(9, 1, "m", a.token, None).unwrap();
    let g = grants(&h.events());
    assert_eq!(g.len(), 1);
    h.release(10, 2, "m", g[0].2.token, None).unwrap();
    assert!(h.events().is_empty());
}

#[test]
fn cancel_dequeues_a_waiter() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 10_000, 0)));
    queued(h.acquire(1, req(2, 5, "m", Want::Mutex, 10_000, 10_000)));
    assert_eq!(h.cancel(2, 2, 5), CancelOutcome::Dequeued);
    assert_eq!(h.cancel(2, 2, 5), CancelOutcome::NotFound);
    h.release(3, 1, "m", a.token, None).unwrap();
    assert!(h.events().is_empty());
    assert_eq!(h.t.stats().entries, 0);
}

#[test]
fn cancel_after_the_grant_raced_in_releases_it() {
    let mut h = H::new(cfg());
    granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 10_000, 0)));
    assert_eq!(h.cancel(1, 1, 1), CancelOutcome::ReleasedGrant);
    granted(h.acquire(2, req(2, 1, "m", Want::Mutex, 10_000, 0)));
}

#[test]
fn close_session_releases_cancels_tags_abandoned_and_skips_lock_delay() {
    let mut h = H::new(Config {
        lock_delay: ms(500),
        ..Config::default()
    });
    let a = granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 10_000, 0)));
    granted(h.acquire(0, req(9, 1, "n", Want::Mutex, 10_000, 0)));
    queued(h.acquire(1, req(1, 2, "n", Want::Mutex, 10_000, 10_000))); // s1 also waits on n
    queued(h.acquire(1, req(2, 1, "m", Want::Mutex, 10_000, 10_000))); // s2 waits on m

    h.close(10, 1);
    let g = grants(&h.events());
    assert_eq!(g.len(), 1, "graceful close: no lock-delay for s2");
    assert_eq!(g[0].0, s(2));
    assert_eq!(
        g[0].2.previous.map(|p| (p.state, p.set_by)),
        Some((HolderState::Abandoned, a.token))
    );
    assert_eq!(h.view(11, 1), SessionView::default());

    // s1's pending request on n is gone: releasing n grants nobody.
    let n = h.view(11, 9).held[0].token;
    h.release(12, 9, "n", n, None).unwrap();
    assert!(h.events().is_empty());
}

#[test]
fn revoke_session_is_close_session_plus_the_lock_delay_of_an_abnormal_loss() {
    let mut h = H::new(Config {
        lock_delay: ms(500),
        ..Config::default()
    });
    let a = granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 10_000, 0)));
    granted(h.acquire(0, req(9, 1, "n", Want::Mutex, 10_000, 0)));
    queued(h.acquire(1, req(1, 2, "n", Want::Mutex, 10_000, 10_000))); // s1 also waits on n
    queued(h.acquire(1, req(2, 1, "m", Want::Mutex, 10_000, 10_000))); // s2 waits on m

    h.t.revoke_session(ms(100), s(1));
    h.check();
    assert_eq!(
        h.view(100, 1),
        SessionView::default(),
        "nothing of s1 is left"
    );
    assert!(
        h.events().is_empty(),
        "m is held back by the lock-delay, so s2 is not granted yet"
    );

    // After the delay s2 gets the lock, tagged as abandoned by the revoked holder.
    let ev = h.poll(600);
    let g = grants(&ev);
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].0, s(2));
    assert_eq!(
        g[0].2.previous.map(|p| (p.state, p.set_by)),
        Some((HolderState::Abandoned, a.token))
    );

    // s1's pending request on n was cancelled: releasing n grants nobody.
    let n = h.view(601, 9).held[0].token;
    h.release(601, 9, "n", n, None).unwrap();
    assert!(h.events().is_empty());
}

#[test]
fn revoking_an_unknown_session_does_nothing() {
    let mut h = H::new(cfg());
    h.t.revoke_session(ms(0), s(42));
    h.check();
}

#[test]
fn fencing_tokens_keep_increasing_when_names_are_garbage_collected() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "gc", Want::Mutex, 1_000, 0)));
    h.release(1, 1, "gc", a.token, None).unwrap();
    assert_eq!(h.t.stats().entries, 0);
    let b = granted(h.acquire(2, req(1, 2, "gc", Want::Mutex, 1_000, 0)));
    assert!(
        b.token > a.token,
        "a per-lock counter would have restarted here"
    );
}

#[test]
fn token_floor_seeds_the_counter() {
    let mut h = H::new(Config {
        token_floor: 1_000_000,
        ..cfg()
    });
    assert_eq!(
        granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 1_000, 0))).token,
        FencingToken(1_000_000)
    );
}

#[test]
fn session_view_reports_held_and_pending_for_reclaim() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "a", Want::Mutex, 5_000, 0)));
    granted(h.acquire(0, req(2, 1, "b", Want::Mutex, 5_000, 0)));
    queued(h.acquire(0, req(1, 2, "b", Want::Mutex, 5_000, 60_000)));

    let view = h.view(1_000, 1);
    assert_eq!(
        view.held,
        vec![HeldGrant {
            request: r(1),
            name: "a".into(),
            want: Want::Mutex,
            token: a.token,
            ttl_remaining: ms(4_000),
            previous: None,
        }]
    );
    assert_eq!(
        view.pending,
        vec![PendingRequest {
            request: r(2),
            name: "b".into(),
            want: Want::Mutex
        }]
    );
    assert_eq!(h.view(1_000, 42), SessionView::default());
}

#[test]
fn session_has_requests_tracks_pending_and_held() {
    let mut h = H::new(cfg());
    assert!(!h.t.session_has_requests(s(1)));
    let a = granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 1_000, 0)));
    assert!(h.t.session_has_requests(s(1)));
    queued(h.acquire(0, req(2, 1, "m", Want::Mutex, 5_000, 500)));
    assert!(h.t.session_has_requests(s(2)), "pending counts");
    h.release(1, 1, "m", a.token, None).unwrap();
    assert!(!h.t.session_has_requests(s(1)));
    h.poll(10_000); // s2 was granted, then its lease expired
    assert!(!h.t.session_has_requests(s(2)));
}

#[test]
fn session_view_carries_the_previous_tag_so_a_reclaimed_grant_is_not_blind() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "job", Want::Mutex, 5_000, 0)));
    h.release(1, 1, "job", a.token, Some(ReleaseState::Succeeded))
        .unwrap();
    let b = granted(h.acquire(2, req(2, 1, "job", Want::Mutex, 5_000, 0)));
    let held = h.view(10, 2).held.remove(0);
    assert_eq!(held.token, b.token);
    assert_eq!(
        held.previous.map(|p| (p.state, p.set_by)),
        Some((HolderState::Succeeded, a.token))
    );
}

#[test]
fn only_the_owning_session_with_the_right_token_can_release() {
    let mut h = H::new(cfg());
    let a = granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 5_000, 0)));
    assert_eq!(h.release(1, 2, "m", a.token, None), Err(Error::NotHeld));
    assert_eq!(
        h.release(1, 1, "m", FencingToken(a.token.0 + 1), None),
        Err(Error::NotHeld)
    );
    assert_eq!(h.release(1, 1, "nope", a.token, None), Err(Error::NotHeld));
    h.release(1, 1, "m", a.token, None).unwrap();
    assert_eq!(
        h.release(2, 1, "m", a.token, None),
        Err(Error::NotHeld),
        "double release"
    );
}

// ------------------------------------------------------------------ limits and restart

#[test]
fn startup_grace_refuses_acquires_until_old_leases_could_have_drained() {
    let mut h = H::new(Config {
        startup_grace: ms(1_000),
        ..cfg()
    });
    assert_eq!(
        h.acquire(999, req(1, 1, "m", Want::Mutex, 1_000, 0)),
        Err(Error::GracePeriod)
    );
    granted(h.acquire(1_000, req(1, 1, "m", Want::Mutex, 1_000, 0)));
}

#[test]
fn limits_are_enforced_without_leaking_state() {
    let mut h = H::new(Config {
        max_waiters_per_lock: 1,
        max_requests_per_session: 2,
        ..cfg()
    });
    granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 5_000, 0)));
    queued(h.acquire(0, req(2, 1, "m", Want::Mutex, 5_000, 5_000)));
    assert!(matches!(
        h.acquire(0, req(3, 1, "m", Want::Mutex, 5_000, 5_000)),
        Err(Error::ResourceExhausted(_))
    ));
    granted(h.acquire(0, req(1, 2, "x", Want::Mutex, 5_000, 0)));
    assert!(matches!(
        h.acquire(0, req(1, 3, "y", Want::Mutex, 5_000, 0)),
        Err(Error::ResourceExhausted(_))
    ));
    assert_eq!(
        h.t.stats().entries,
        2,
        "rejected requests created no entries"
    );
}

#[test]
fn names_and_ttls_are_validated() {
    let mut h = H::new(cfg());
    assert!(matches!(
        h.acquire(0, req(1, 1, "", Want::Mutex, 1_000, 0)),
        Err(Error::InvalidArgument(_))
    ));
    let long = "x".repeat(257);
    assert!(matches!(
        h.acquire(0, req(1, 1, &long, Want::Mutex, 1_000, 0)),
        Err(Error::InvalidArgument(_))
    ));
    assert!(matches!(
        h.acquire(0, req(1, 1, "m", Want::Mutex, 0, 0)),
        Err(Error::InvalidArgument(_))
    ));
}

#[test]
fn everything_drains_to_empty() {
    let mut h = H::new(Config {
        lock_delay: ms(100),
        default_state_ttl: ms(100),
        ..Config::default()
    });
    let a = granted(h.acquire(0, req(1, 1, "m", Want::Mutex, 50, 0)));
    queued(h.acquire(1, req(2, 1, "m", Want::Mutex, 50, 1_000)));
    h.release(2, 2, "m", a.token, None).unwrap_err(); // wrong session
    h.poll(10_000);
    h.close(10_000, 1);
    h.close(10_000, 2);
    h.poll(100_000);
    assert_eq!(
        h.t.stats(),
        Stats {
            entries: 0,
            sessions: 0,
            timers: 0
        }
    );
}
