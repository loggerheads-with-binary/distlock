//! Random operation sequences against the lock table. After every step the table's full
//! invariant check runs (mutual exclusion per kind, no missed wakeups, index ↔ queue consistency,
//! no past-due leases, every lease and waiter has a timer, no idle residue), plus a few
//! externally visible properties. At the end everything must drain to nothing (no leaks).

use std::time::Duration;

use distlock_core::*;
use proptest::prelude::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[derive(Debug, Clone)]
enum Op {
    Acquire {
        session: u8,
        which: u8,
        variant: u8,
        ttl: u16,
        wait: u16,
    },
    Release {
        session: u8,
        pick: u8,
        tag: u8,
    },
    Extend {
        session: u8,
        pick: u8,
        ttl: u16,
    },
    Cancel {
        session: u8,
        pick: u8,
    },
    Close {
        session: u8,
    },
    Revoke {
        session: u8,
    },
    Tick {
        by: u16,
    },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (1u8..=3, 0u8..3, 0u8..3, 1u16..3_000, 0u16..3_000)
            .prop_map(|(session, which, variant, ttl, wait)| Op::Acquire { session, which, variant, ttl, wait }),
        3 => (1u8..=3, any::<u8>(), 0u8..4)
            .prop_map(|(session, pick, tag)| Op::Release { session, pick, tag }),
        2 => (1u8..=3, any::<u8>(), 1u16..4_000)
            .prop_map(|(session, pick, ttl)| Op::Extend { session, pick, ttl }),
        1 => (1u8..=3, any::<u8>()).prop_map(|(session, pick)| Op::Cancel { session, pick }),
        1 => (1u8..=3).prop_map(|session| Op::Close { session }),
        1 => (1u8..=3).prop_map(|session| Op::Revoke { session }),
        4 => (1u16..1_500).prop_map(|by| Op::Tick { by }),
    ]
}

const MAX_TTL_MS: u64 = 5_000;

fn run(ops: Vec<Op>) {
    let cfg = Config {
        max_ttl: ms(MAX_TTL_MS),
        max_hold: Some(ms(8_000)),
        lock_delay: ms(200),
        default_state_ttl: ms(700),
        max_waiters_per_lock: 4,
        max_requests_per_session: 6,
        ..Config::default()
    };
    let mut table = LockTable::new(cfg);
    let mut now = 0u64;
    let mut next_request = 1u64;
    let mut last_token = 0u64;

    for op in ops {
        // Tokens must come out strictly increasing in the order they are issued:
        // first whatever time-driven events granted, then the call's own immediate grant.
        let mut issued: Vec<u64> = Vec::new();

        match op {
            Op::Acquire {
                session,
                which,
                variant,
                ttl,
                wait,
            } => {
                let (name, want) = match which {
                    0 => ("m", Want::Mutex),
                    1 => (
                        "rw",
                        if variant == 1 {
                            Want::Write
                        } else {
                            Want::Read
                        },
                    ),
                    _ => (
                        "sem",
                        Want::Semaphore {
                            capacity: 2,
                            permits: 1 + u32::from(variant % 2),
                        },
                    ),
                };
                let request = RequestId(next_request);
                next_request += 1;
                let res = table.acquire(
                    ms(now),
                    AcquireRequest {
                        session: SessionId(u64::from(session)),
                        request,
                        name: name.into(),
                        want,
                        ttl: ms(u64::from(ttl)),
                        wait: ms(u64::from(wait)),
                    },
                );
                // Drain time-driven events first (they were issued before the immediate grant).
                for e in table.take_events() {
                    if let Event::Granted { grant, .. } = e {
                        issued.push(grant.token.0);
                    }
                }
                match res {
                    Ok(AcquireStatus::Granted(g)) => {
                        issued.push(g.token.0);
                        assert!(g.ttl_remaining <= ms(MAX_TTL_MS));
                    }
                    Ok(AcquireStatus::Queued)
                    | Err(Error::Busy)
                    | Err(Error::ResourceExhausted(_)) => {}
                    Err(e) => panic!("unexpected acquire error: {e}"),
                }
            }
            Op::Release { session, pick, tag } => {
                let sid = SessionId(u64::from(session));
                let view = table.session_view(ms(now), sid);
                if !view.held.is_empty() {
                    let h = &view.held[usize::from(pick) % view.held.len()];
                    let state = match tag {
                        1 => Some(ReleaseState::Succeeded),
                        2 => Some(ReleaseState::Failed),
                        3 => Some(ReleaseState::Stored),
                        _ => None,
                    };
                    match table.release(ms(now), sid, &h.name, h.token, state, None) {
                        Ok(()) => {}
                        Err(Error::InvalidArgument(_)) => {
                            assert!(!matches!(h.want, Want::Mutex | Want::Write))
                        }
                        Err(e) => panic!("unexpected release error: {e}"),
                    }
                }
            }
            Op::Extend { session, pick, ttl } => {
                let sid = SessionId(u64::from(session));
                let view = table.session_view(ms(now), sid);
                if !view.held.is_empty() {
                    let h = &view.held[usize::from(pick) % view.held.len()];
                    let remaining = table
                        .extend(ms(now), sid, &h.name, h.token, ms(u64::from(ttl)))
                        .expect("a lease the table just reported as held must be extendable");
                    assert!(
                        remaining >= h.ttl_remaining,
                        "extend must never shorten a lease"
                    );
                    assert!(remaining <= ms(MAX_TTL_MS));
                }
            }
            Op::Cancel { session, pick } => {
                let sid = SessionId(u64::from(session));
                let view = table.session_view(ms(now), sid);
                let mut requests: Vec<RequestId> = view.held.iter().map(|h| h.request).collect();
                requests.extend(view.pending.iter().map(|p| p.request));
                if !requests.is_empty() {
                    let req = requests[usize::from(pick) % requests.len()];
                    assert_ne!(table.cancel(ms(now), sid, req), CancelOutcome::NotFound);
                }
            }
            Op::Close { session } => table.close_session(ms(now), SessionId(u64::from(session))),
            Op::Revoke { session } => table.revoke_session(ms(now), SessionId(u64::from(session))),
            Op::Tick { by } => {
                now += u64::from(by);
                table.poll(ms(now));
            }
        }

        for e in table.take_events() {
            if let Event::Granted { grant, .. } = e {
                issued.push(grant.token.0);
            }
        }
        for token in issued {
            assert!(
                token > last_token,
                "fencing token {token} not greater than {last_token}"
            );
            last_token = token;
        }
        table
            .check_invariants()
            .unwrap_or_else(|e| panic!("invariant violated: {e}"));

        for session in 1..=3u64 {
            for held in table.session_view(ms(now), SessionId(session)).held {
                assert!(
                    held.ttl_remaining <= ms(MAX_TTL_MS),
                    "lease longer than max_ttl"
                );
                assert!(
                    !held.ttl_remaining.is_zero(),
                    "past-due lease still reported as held"
                );
            }
        }
    }

    // Everything must drain: close every session, let all timers run out, nothing may remain.
    for session in 1..=3u64 {
        table.close_session(ms(now), SessionId(session));
    }
    table.poll(ms(now + 86_400_000));
    table.check_invariants().unwrap();
    assert_eq!(
        table.stats(),
        Stats {
            entries: 0,
            sessions: 0,
            timers: 0
        },
        "state leaked"
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn invariants_hold_under_random_operations(ops in prop::collection::vec(op(), 1..150)) {
        run(ops);
    }
}
