//! The lock table: a deterministic, synchronous state machine.
//!
//! Nothing in here does IO or reads a clock. Every operation takes `now` (monotonic time since
//! an arbitrary epoch). Timers (lease expiry, wait timeouts, lock-delay, name garbage collection)
//! are kept in a `BTreeMap` and fired, in deadline order, at the start of every operation.
//!
//! Structure of one operation that touches a name:
//!
//! 1. `advance(now)`: fire all due timers.
//! 2. Take the `Entry` *out* of the map, mutate it freely alongside `&mut self`.
//! 3. `dispatch` grants to queue heads while they are admissible (strict FIFO).
//! 4. `put_entry` puts it back, or drops it if idle with no residue (and schedules GC otherwise).
//!
//! Events (named broadcasts) live in their own map, `event_table`, and follow the same pattern with
//! `put_event`. They share the session index, the timers and the request-id space with locks.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use crate::types::*;

#[derive(Debug, Clone)]
pub struct Config {
    /// Cap on a lease length, both at grant and per `extend`.
    pub max_ttl: Duration,
    /// Cap on how long an acquire may wait.
    pub max_wait: Duration,
    /// Cap on the *total* life of one grant across extensions. `None` = unbounded.
    pub max_hold: Option<Duration>,
    /// After an abnormal loss (expiry) of a Mutex/Write grant, nobody is granted the name for this long.
    pub lock_delay: Duration,
    /// How long a holder state tag stays readable when the releaser does not say.
    pub default_state_ttl: Duration,
    pub max_state_ttl: Duration,
    pub max_name_len: usize,
    pub max_waiters_per_lock: usize,
    /// Outstanding (pending + held) requests per session.
    pub max_requests_per_session: usize,
    /// Refuse acquires until the table clock reaches this value. A restarted memory-only server
    /// sets it to `max_ttl` so leases granted by its previous incarnation drain first.
    pub startup_grace: Duration,
    /// First fencing token. Seed from the wall clock so tokens stay monotonic across restarts.
    pub token_floor: u64,
    /// How long an event stays published after a `set` that does not say.
    pub event_default_publish: Duration,
    /// Cap on how long an event may stay published after a `set`.
    pub event_max_publish: Duration,
    /// Longest value a `set` may carry, in bytes.
    pub max_event_value_len: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_ttl: Duration::from_secs(60),
            max_wait: Duration::from_secs(300),
            max_hold: None,
            lock_delay: Duration::from_secs(5),
            default_state_ttl: Duration::from_secs(600),
            max_state_ttl: Duration::from_secs(24 * 3600),
            max_name_len: 256,
            max_waiters_per_lock: 10_000,
            max_requests_per_session: 10_000,
            startup_grace: Duration::ZERO,
            token_floor: 1,
            event_default_publish: Duration::from_secs(10),
            event_max_publish: Duration::from_secs(600),
            max_event_value_len: 4096,
        }
    }
}

#[derive(Debug)]
struct Holder {
    token: FencingToken,
    session: SessionId,
    request: RequestId,
    want: Want,
    granted_at: Time,
    expires_at: Time,
}

#[derive(Debug)]
struct Waiter {
    session: SessionId,
    request: RequestId,
    want: Want,
    ttl: Duration,
    deadline: Time,
}

#[derive(Debug, Clone, Copy)]
struct StoredState {
    state: HolderState,
    set_by: FencingToken,
    set_at: Time,
    expires_at: Time,
}

#[derive(Debug)]
struct Entry {
    kind: Kind,
    holders: Vec<Holder>,
    queue: VecDeque<Waiter>,
    state: Option<StoredState>,
    /// Nobody is granted before this time (lock-delay). `ZERO` = no delay.
    delay_until: Time,
    /// When the outstanding GC timer for this entry fires, if any. Keeps timers O(1) per entry.
    gc_timer_at: Option<Time>,
}

impl Entry {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            holders: Vec::new(),
            queue: VecDeque::new(),
            state: None,
            delay_until: Time::ZERO,
            gc_timer_at: None,
        }
    }

    fn used_permits(&self) -> u64 {
        self.holders.iter().map(|h| h.want.permits()).sum()
    }

    /// Would `want` be admissible *right now*, ignoring the queue and lock-delay?
    fn can_admit(&self, want: Want) -> bool {
        match want {
            Want::Mutex | Want::Write => self.holders.is_empty(),
            Want::Read => self.holders.iter().all(|h| h.want == Want::Read),
            Want::Semaphore { capacity, permits } => {
                self.used_permits() + u64::from(permits) <= u64::from(capacity)
            }
        }
    }

    fn previous(&self, now: Time) -> Option<PreviousState> {
        self.state
            .filter(|s| s.expires_at > now)
            .map(|s| PreviousState {
                state: s.state,
                set_by: s.set_by,
                age: now.saturating_sub(s.set_at),
            })
    }
}

#[derive(Debug)]
struct EventWaiter {
    session: SessionId,
    request: RequestId,
    deadline: Time,
}

/// The latest setting of an event, while it is inside its publish window.
#[derive(Debug, Clone)]
struct Published {
    id: u64,
    value: Option<Arc<str>>,
    set_at: Time,
    until: Time,
}

/// One event name: who is waiting for it, and its latest setting while that is still published.
#[derive(Debug, Default)]
struct EventState {
    waiters: VecDeque<EventWaiter>,
    published: Option<Published>,
    /// When the outstanding expiry timer for this name fires, if any. Keeps timers O(1) per name.
    timer_at: Option<Time>,
}

#[derive(Debug)]
enum Timer {
    /// Lease expiry. Re-arms itself if the lease was extended (so extend never touches timers).
    Expire { name: Name, token: FencingToken },
    Wait {
        name: Name,
        session: SessionId,
        request: RequestId,
    },
    /// Lock-delay end / state-tag expiry: re-dispatch the queue and garbage-collect.
    Entry { name: Name },
    /// An event wait ran out of patience.
    EventWait {
        name: Name,
        session: SessionId,
        request: RequestId,
    },
    /// An event's publish window ended: forget it, and garbage-collect the name.
    EventExpire { name: Name },
}

/// Every live request of a session, by request id: locks (pending or held) and event waits.
/// One id space: a request id is in at most one of the two maps.
#[derive(Debug, Default)]
struct SessionIndex {
    requests: HashMap<RequestId, Name>,
    events: HashMap<RequestId, Name>,
}

impl SessionIndex {
    fn outstanding(&self) -> usize {
        self.requests.len() + self.events.len()
    }

    fn is_empty(&self) -> bool {
        self.requests.is_empty() && self.events.is_empty()
    }
}

#[derive(Debug)]
pub struct LockTable {
    cfg: Config,
    now: Time,
    entries: HashMap<Name, Entry>,
    event_table: HashMap<Name, EventState>,
    sessions: HashMap<SessionId, SessionIndex>,
    timers: BTreeMap<(Time, u64), Timer>,
    timer_seq: u64,
    next_token: u64,
    events: Vec<Event>,
}

impl LockTable {
    pub fn new(cfg: Config) -> Self {
        let next_token = cfg.token_floor.max(1);
        Self {
            cfg,
            now: Time::ZERO,
            entries: HashMap::new(),
            event_table: HashMap::new(),
            sessions: HashMap::new(),
            timers: BTreeMap::new(),
            timer_seq: 0,
            next_token,
            events: Vec::new(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// When the next timer is due. The actor sleeps until then and calls [`poll`](Self::poll).
    pub fn next_deadline(&self) -> Option<Time> {
        self.timers.keys().next().map(|(at, _)| *at)
    }

    /// Fire due timers. Cheap to call; every other operation does this first.
    pub fn poll(&mut self, now: Time) {
        self.advance(now);
    }

    /// Drain side effects. Call after every operation, including `poll`.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    pub fn stats(&self) -> Stats {
        Stats {
            entries: self.entries.len(),
            events: self.event_table.len(),
            sessions: self.sessions.len(),
            timers: self.timers.len(),
        }
    }

    /// Whether `session` has any pending or held request. A session with none (and no connections)
    /// can be forgotten by the layer above without telling the table.
    pub fn session_has_requests(&self, session: SessionId) -> bool {
        self.sessions.contains_key(&session)
    }

    // ------------------------------------------------------------------ operations

    pub fn acquire(&mut self, now: Time, req: AcquireRequest) -> Result<AcquireStatus, Error> {
        self.advance(now);
        let now = self.now;
        self.validate(&req)?;
        if now < self.cfg.startup_grace {
            return Err(Error::GracePeriod);
        }

        // Idempotent retry of a request we already know about.
        let known = self
            .sessions
            .get(&req.session)
            .and_then(|s| s.requests.get(&req.request))
            .cloned();
        if let Some(known_name) = known {
            return self.answer_retry(&req, &known_name);
        }
        if self
            .sessions
            .get(&req.session)
            .is_some_and(|s| s.events.contains_key(&req.request))
        {
            return Err(Error::RequestConflict);
        }

        let kind = req.want.kind();
        if let Some(existing) = self.entries.get(&req.name)
            && existing.kind != kind
        {
            return Err(match (existing.kind, kind) {
                (Kind::Semaphore { capacity }, Kind::Semaphore { .. }) => {
                    Error::CapacityMismatch { existing: capacity }
                }
                (existing, _) => Error::KindMismatch { existing },
            });
        }
        let outstanding = self
            .sessions
            .get(&req.session)
            .map_or(0, SessionIndex::outstanding);
        if outstanding >= self.cfg.max_requests_per_session {
            return Err(Error::ResourceExhausted(
                "too many outstanding requests for session",
            ));
        }

        let wait = req.wait.min(self.cfg.max_wait);
        let (name, mut entry) = self
            .entries
            .remove_entry(&req.name)
            .unwrap_or_else(|| (req.name.clone(), Entry::new(kind)));

        let free = entry.queue.is_empty() && entry.delay_until <= now && entry.can_admit(req.want);
        let result = if free {
            let info = self.grant(
                &name,
                &mut entry,
                req.session,
                req.request,
                req.want,
                req.ttl,
            );
            self.index(req.session, req.request, name.clone());
            Ok(AcquireStatus::Granted(info))
        } else if wait.is_zero() {
            Err(Error::Busy)
        } else if entry.queue.len() >= self.cfg.max_waiters_per_lock {
            Err(Error::ResourceExhausted("wait queue is full"))
        } else {
            let deadline = now + wait;
            entry.queue.push_back(Waiter {
                session: req.session,
                request: req.request,
                want: req.want,
                ttl: req.ttl,
                deadline,
            });
            self.schedule(
                deadline,
                Timer::Wait {
                    name: name.clone(),
                    session: req.session,
                    request: req.request,
                },
            );
            self.index(req.session, req.request, name.clone());
            Ok(AcquireStatus::Queued)
        };
        self.put_entry(name, entry);
        result
    }

    /// Cancel a request. Waiting → dequeued (an event wait too). Already granted → the grant is
    /// released, which is what a client whose `acquire` future was dropped wants (no lock is ever
    /// granted to nobody).
    pub fn cancel(&mut self, now: Time, session: SessionId, request: RequestId) -> CancelOutcome {
        self.advance(now);
        let Some(known_name) = self
            .sessions
            .get(&session)
            .and_then(|s| s.requests.get(&request))
            .cloned()
        else {
            return self.cancel_event_wait(session, request);
        };
        let Some((name, mut entry)) = self.entries.remove_entry(&known_name) else {
            return CancelOutcome::NotFound;
        };
        let outcome = if let Some(pos) = entry
            .queue
            .iter()
            .position(|w| w.session == session && w.request == request)
        {
            entry.queue.remove(pos);
            CancelOutcome::Dequeued
        } else if let Some(pos) = entry
            .holders
            .iter()
            .position(|h| h.session == session && h.request == request)
        {
            entry.holders.swap_remove(pos);
            CancelOutcome::ReleasedGrant
        } else {
            CancelOutcome::NotFound
        };
        self.forget(session, request);
        self.dispatch(&name, &mut entry);
        self.put_entry(name, entry);
        outcome
    }

    /// Release a grant. `state` (Mutex/Write holders only) is the tag the next acquirer will see;
    /// `None` leaves the existing tag untouched. `state_ttl` of `None`/zero means the server default.
    pub fn release(
        &mut self,
        now: Time,
        session: SessionId,
        name: &str,
        token: FencingToken,
        state: Option<ReleaseState>,
        state_ttl: Option<Duration>,
    ) -> Result<(), Error> {
        self.advance(now);
        let pos = {
            let entry = self.entries.get(name).ok_or(Error::NotHeld)?;
            let pos = entry
                .holders
                .iter()
                .position(|h| h.token == token && h.session == session)
                .ok_or(Error::NotHeld)?;
            if state.is_some() && !entry.holders[pos].want.is_exclusive() {
                return Err(Error::InvalidArgument(
                    "a state tag can only be set by a mutex or write holder",
                ));
            }
            pos
        };
        let Some((name, mut entry)) = self.entries.remove_entry(name) else {
            return Err(Error::Internal("entry vanished during release"));
        };
        let holder = entry.holders.swap_remove(pos);
        self.forget(holder.session, holder.request);
        if let Some(state) = state {
            self.set_state(&mut entry, token, state.into(), state_ttl);
        }
        self.dispatch(&name, &mut entry);
        self.put_entry(name, entry);
        Ok(())
    }

    /// Push a lease's expiry out to `now + ttl` (never shortens it; capped by `max_ttl` per call and
    /// `max_hold` in total). Returns the remaining lease. Fails with `NotHeld` once the lease is
    /// gone: expiry is never undone.
    pub fn extend(
        &mut self,
        now: Time,
        session: SessionId,
        name: &str,
        token: FencingToken,
        ttl: Duration,
    ) -> Result<Duration, Error> {
        self.advance(now);
        let now = self.now;
        if ttl.is_zero() {
            return Err(Error::InvalidArgument("ttl must be greater than zero"));
        }
        let ttl = ttl.min(self.cfg.max_ttl);
        let max_hold = self.cfg.max_hold;
        let entry = self.entries.get_mut(name).ok_or(Error::NotHeld)?;
        let holder = entry
            .holders
            .iter_mut()
            .find(|h| h.token == token && h.session == session)
            .ok_or(Error::NotHeld)?;
        let mut expires_at = now + ttl;
        if let Some(max_hold) = max_hold {
            expires_at = expires_at.min(holder.granted_at + max_hold);
        }
        // The Expire timer fires at or before `expires_at` and re-arms itself, so no timer work here.
        holder.expires_at = holder.expires_at.max(expires_at);
        Ok(holder.expires_at.saturating_sub(now))
    }

    /// Graceful session end: cancel everything pending, release everything held (tag `Abandoned` for
    /// exclusive holders, no lock-delay).
    pub fn close_session(&mut self, now: Time, session: SessionId) {
        self.end_session(now, session, false);
    }

    /// Forced session end, for a client that is being thrown out (its credentials were revoked).
    /// Like [`close_session`](Self::close_session), but treated as an *abnormal* loss: after each
    /// exclusive grant it held, the name is not granted to anyone for `lock_delay`, because the
    /// revoked process may still be inside its critical section until it notices.
    pub fn revoke_session(&mut self, now: Time, session: SessionId) {
        self.end_session(now, session, true);
    }

    fn end_session(&mut self, now: Time, session: SessionId, forced: bool) {
        self.advance(now);
        let now = self.now;
        let Some(SessionIndex { requests, events }) = self.sessions.remove(&session) else {
            return;
        };
        let mut event_names: Vec<Name> = events.into_values().collect();
        event_names.sort();
        event_names.dedup();
        for name in event_names {
            let Some((name, mut state)) = self.event_table.remove_entry(&name) else {
                continue;
            };
            state.waiters.retain(|w| w.session != session);
            self.put_event(name, state);
        }
        let mut names: Vec<Name> = requests.into_values().collect();
        names.sort();
        names.dedup();
        for name in names {
            let Some((name, mut entry)) = self.entries.remove_entry(&name) else {
                continue;
            };
            entry.queue.retain(|w| w.session != session);
            let mut i = 0;
            while i < entry.holders.len() {
                if entry.holders[i].session == session {
                    let holder = entry.holders.swap_remove(i);
                    if holder.want.is_exclusive() {
                        self.set_state(&mut entry, holder.token, HolderState::Abandoned, None);
                        if forced && !self.cfg.lock_delay.is_zero() {
                            entry.delay_until = entry.delay_until.max(now + self.cfg.lock_delay);
                            self.schedule(entry.delay_until, Timer::Entry { name: name.clone() });
                        }
                    }
                } else {
                    i += 1;
                }
            }
            self.dispatch(&name, &mut entry);
            self.put_entry(name, entry);
        }
    }

    /// What the table believes `session` holds and waits for. This is the *reclaim* answer.
    pub fn session_view(&mut self, now: Time, session: SessionId) -> SessionView {
        self.advance(now);
        let now = self.now;
        let mut view = SessionView::default();
        let Some(index) = self.sessions.get(&session) else {
            return view;
        };
        for (request, name) in &index.requests {
            let Some(entry) = self.entries.get(name) else {
                continue;
            };
            if let Some(h) = entry
                .holders
                .iter()
                .find(|h| h.session == session && h.request == *request)
            {
                view.held.push(HeldGrant {
                    request: *request,
                    name: name.clone(),
                    want: h.want,
                    token: h.token,
                    ttl_remaining: h.expires_at.saturating_sub(now),
                    // Nobody can change the tag while this grant is held (exclusive holders are
                    // alone; readers exclude writers), so this is what the grant reported.
                    previous: entry.previous(now),
                });
            } else if let Some(w) = entry
                .queue
                .iter()
                .find(|w| w.session == session && w.request == *request)
            {
                view.pending.push(PendingRequest {
                    request: *request,
                    name: name.clone(),
                    want: w.want,
                });
            }
        }
        for (request, name) in &index.events {
            let waiting = self.event_table.get(name).is_some_and(|e| {
                e.waiters
                    .iter()
                    .any(|w| w.session == session && w.request == *request)
            });
            if waiting {
                view.pending_events.push(PendingEvent {
                    request: *request,
                    name: name.clone(),
                });
            }
        }
        view.held.sort_by_key(|h| h.token);
        view.pending.sort_by_key(|p| p.request);
        view.pending_events.sort_by_key(|p| p.request);
        view
    }

    // ------------------------------------------------------------------ internals

    fn validate(&self, req: &AcquireRequest) -> Result<(), Error> {
        if req.name.is_empty() || req.name.len() > self.cfg.max_name_len {
            return Err(Error::InvalidArgument("name length out of range"));
        }
        if req.ttl.is_zero() {
            return Err(Error::InvalidArgument("ttl must be greater than zero"));
        }
        if let Want::Semaphore { capacity, permits } = req.want
            && (capacity == 0 || permits == 0 || permits > capacity)
        {
            return Err(Error::InvalidArgument(
                "semaphore needs capacity > 0 and 0 < permits <= capacity",
            ));
        }
        Ok(())
    }

    fn answer_retry(
        &self,
        req: &AcquireRequest,
        known_name: &Name,
    ) -> Result<AcquireStatus, Error> {
        let now = self.now;
        if *known_name != req.name {
            return Err(Error::RequestConflict);
        }
        let entry = self
            .entries
            .get(known_name)
            .ok_or(Error::Internal("indexed request has no entry"))?;
        if let Some(h) = entry
            .holders
            .iter()
            .find(|h| h.session == req.session && h.request == req.request)
        {
            if h.want != req.want {
                return Err(Error::RequestConflict);
            }
            return Ok(AcquireStatus::Granted(GrantInfo {
                token: h.token,
                ttl_remaining: h.expires_at.saturating_sub(now),
                previous: entry.previous(now),
            }));
        }
        if let Some(w) = entry
            .queue
            .iter()
            .find(|w| w.session == req.session && w.request == req.request)
        {
            if w.want != req.want {
                return Err(Error::RequestConflict);
            }
            return Ok(AcquireStatus::Queued);
        }
        Err(Error::Internal(
            "indexed request is neither held nor pending",
        ))
    }

    /// Fire every timer due at or before `target`, in deadline order, with the table clock set to
    /// each timer's own deadline while it runs (so a long gap between calls behaves exactly like
    /// frequent polling would have).
    fn advance(&mut self, target: Time) {
        let target = target.max(self.now);
        while let Some(first) = self.timers.first_entry() {
            if first.key().0 > target {
                break;
            }
            let ((at, _), timer) = first.remove_entry();
            self.now = self.now.max(at);
            match timer {
                Timer::Expire { name, token } => self.on_expire(name, token),
                Timer::Wait {
                    name,
                    session,
                    request,
                } => self.on_wait_timeout(name, session, request),
                Timer::Entry { name } => self.on_entry_timer(name),
                Timer::EventWait {
                    name,
                    session,
                    request,
                } => self.on_event_wait_timeout(name, session, request),
                Timer::EventExpire { name } => self.on_event_timer(name),
            }
        }
        self.now = target;
    }

    fn on_expire(&mut self, name: Name, token: FencingToken) {
        let now = self.now;
        let Some(mut entry) = self.entries.remove(&name) else {
            return;
        };
        if let Some(pos) = entry.holders.iter().position(|h| h.token == token) {
            let expires_at = entry.holders[pos].expires_at;
            if expires_at > now {
                // Extended since this timer was set: re-arm at the new expiry.
                self.schedule(
                    expires_at,
                    Timer::Expire {
                        name: name.clone(),
                        token,
                    },
                );
            } else {
                let holder = entry.holders.swap_remove(pos);
                self.forget(holder.session, holder.request);
                self.events.push(Event::LeaseExpired {
                    session: holder.session,
                    name: name.clone(),
                    token,
                });
                if holder.want.is_exclusive() {
                    self.set_state(&mut entry, token, HolderState::Abandoned, None);
                    if !self.cfg.lock_delay.is_zero() {
                        entry.delay_until = now + self.cfg.lock_delay;
                        self.schedule(entry.delay_until, Timer::Entry { name: name.clone() });
                    }
                }
                self.dispatch(&name, &mut entry);
            }
        }
        self.put_entry(name, entry);
    }

    fn on_wait_timeout(&mut self, name: Name, session: SessionId, request: RequestId) {
        let now = self.now;
        let Some(mut entry) = self.entries.remove(&name) else {
            return;
        };
        // `deadline <= now` guards against a stale timer of an earlier incarnation of this request id.
        if let Some(pos) = entry
            .queue
            .iter()
            .position(|w| w.session == session && w.request == request && w.deadline <= now)
        {
            entry.queue.remove(pos);
            self.forget(session, request);
            self.events.push(Event::WaitTimedOut {
                session,
                request,
                name: name.clone(),
            });
            // A timed-out head may have been the only thing blocking the waiters behind it.
            self.dispatch(&name, &mut entry);
        }
        self.put_entry(name, entry);
    }

    fn on_entry_timer(&mut self, name: Name) {
        let Some(mut entry) = self.entries.remove(&name) else {
            return;
        };
        if entry.gc_timer_at.is_some_and(|at| at <= self.now) {
            entry.gc_timer_at = None;
        }
        self.dispatch(&name, &mut entry);
        self.put_entry(name, entry);
    }

    /// Grant to queue heads while admissible. Strict FIFO: a head that does not fit blocks
    /// everyone behind it (no starvation of writers or of many-permit requests).
    fn dispatch(&mut self, name: &Name, entry: &mut Entry) {
        if entry.delay_until > self.now {
            return;
        }
        while let Some(head) = entry.queue.front() {
            if !entry.can_admit(head.want) {
                break;
            }
            let Some(waiter) = entry.queue.pop_front() else {
                break;
            };
            let grant = self.grant(
                name,
                entry,
                waiter.session,
                waiter.request,
                waiter.want,
                waiter.ttl,
            );
            self.events.push(Event::Granted {
                session: waiter.session,
                request: waiter.request,
                name: name.clone(),
                grant,
            });
        }
    }

    fn grant(
        &mut self,
        name: &Name,
        entry: &mut Entry,
        session: SessionId,
        request: RequestId,
        want: Want,
        ttl: Duration,
    ) -> GrantInfo {
        let now = self.now;
        let token = FencingToken(self.next_token);
        self.next_token += 1;
        let mut ttl = ttl.min(self.cfg.max_ttl);
        if let Some(max_hold) = self.cfg.max_hold {
            ttl = ttl.min(max_hold);
        }
        let expires_at = now + ttl;
        entry.holders.push(Holder {
            token,
            session,
            request,
            want,
            granted_at: now,
            expires_at,
        });
        self.schedule(
            expires_at,
            Timer::Expire {
                name: name.clone(),
                token,
            },
        );
        GrantInfo {
            token,
            ttl_remaining: ttl,
            previous: entry.previous(now),
        }
    }

    fn set_state(
        &self,
        entry: &mut Entry,
        by: FencingToken,
        state: HolderState,
        ttl: Option<Duration>,
    ) {
        let ttl = ttl
            .filter(|d| !d.is_zero())
            .unwrap_or(self.cfg.default_state_ttl)
            .min(self.cfg.max_state_ttl);
        entry.state = Some(StoredState {
            state,
            set_by: by,
            set_at: self.now,
            expires_at: self.now + ttl,
        });
    }

    /// Return an entry to the map, or drop it if it is idle with nothing left to remember.
    /// An idle entry with a live state tag or lock-delay stays until both are over (one GC timer).
    fn put_entry(&mut self, name: Name, mut entry: Entry) {
        let now = self.now;
        if entry.holders.is_empty() && entry.queue.is_empty() {
            if entry.state.is_some_and(|s| s.expires_at <= now) {
                entry.state = None;
            }
            let delay_end = (entry.delay_until > now).then_some(entry.delay_until);
            let state_end = entry.state.map(|s| s.expires_at);
            let Some(gc_at) = delay_end.into_iter().chain(state_end).max() else {
                return; // nothing to remember: the name leaves the table
            };
            if entry.gc_timer_at.is_none_or(|scheduled| scheduled > gc_at) {
                entry.gc_timer_at = Some(gc_at);
                self.schedule(gc_at, Timer::Entry { name: name.clone() });
            }
        }
        self.entries.insert(name, entry);
    }

    fn schedule(&mut self, at: Time, timer: Timer) {
        let seq = self.timer_seq;
        self.timer_seq += 1;
        self.timers.insert((at, seq), timer);
    }

    fn index(&mut self, session: SessionId, request: RequestId, name: Name) {
        self.sessions
            .entry(session)
            .or_default()
            .requests
            .insert(request, name);
    }

    fn forget(&mut self, session: SessionId, request: RequestId) {
        if let Some(index) = self.sessions.get_mut(&session) {
            index.requests.remove(&request);
            if index.is_empty() {
                self.sessions.remove(&session);
            }
        }
    }

    fn index_event(&mut self, session: SessionId, request: RequestId, name: Name) {
        self.sessions
            .entry(session)
            .or_default()
            .events
            .insert(request, name);
    }

    fn forget_event(&mut self, session: SessionId, request: RequestId) {
        if let Some(index) = self.sessions.get_mut(&session) {
            index.events.remove(&request);
            if index.is_empty() {
                self.sessions.remove(&session);
            }
        }
    }

    // ------------------------------------------------------------------ events

    /// Wait for an event. If it is inside its publish window (and newer than `req.after`) the
    /// answer is immediate; otherwise the request is registered, and `Event::Published` or
    /// `Event::WaitTimedOut` follows. With a zero `wait`, "nothing published" is `Error::Busy`.
    pub fn event_wait(
        &mut self,
        now: Time,
        req: EventWaitRequest,
    ) -> Result<EventWaitStatus, Error> {
        self.advance(now);
        let now = self.now;
        self.validate_event_name(&req.name)?;

        // Idempotent retry of a wait we already know about.
        let known = self
            .sessions
            .get(&req.session)
            .and_then(|s| s.events.get(&req.request))
            .cloned();
        if let Some(known_name) = known {
            if known_name != req.name {
                return Err(Error::RequestConflict);
            }
            let waiting = self.event_table.get(&known_name).is_some_and(|e| {
                e.waiters
                    .iter()
                    .any(|w| w.session == req.session && w.request == req.request)
            });
            return if waiting {
                Ok(EventWaitStatus::Waiting)
            } else {
                Err(Error::Internal("indexed event wait is not pending"))
            };
        }
        let index = self.sessions.get(&req.session);
        if index.is_some_and(|s| s.requests.contains_key(&req.request)) {
            return Err(Error::RequestConflict);
        }
        if index.map_or(0, SessionIndex::outstanding) >= self.cfg.max_requests_per_session {
            return Err(Error::ResourceExhausted(
                "too many outstanding requests for session",
            ));
        }

        let wait = req.wait.min(self.cfg.max_wait);
        let (name, mut state) = self
            .event_table
            .remove_entry(&req.name)
            .unwrap_or_else(|| (req.name.clone(), EventState::default()));
        let hit = state
            .published
            .as_ref()
            .filter(|p| p.until > now && p.id > req.after)
            .map(|p| Publication {
                id: p.id,
                value: p.value.clone(),
                age: now.saturating_sub(p.set_at),
            });
        let result = if let Some(publication) = hit {
            Ok(EventWaitStatus::Published(publication))
        } else if wait.is_zero() {
            Err(Error::Busy)
        } else if state.waiters.len() >= self.cfg.max_waiters_per_lock {
            Err(Error::ResourceExhausted("event wait queue is full"))
        } else {
            let deadline = now + wait;
            state.waiters.push_back(EventWaiter {
                session: req.session,
                request: req.request,
                deadline,
            });
            self.schedule(
                deadline,
                Timer::EventWait {
                    name: name.clone(),
                    session: req.session,
                    request: req.request,
                },
            );
            self.index_event(req.session, req.request, name.clone());
            Ok(EventWaitStatus::Waiting)
        };
        self.put_event(name, state);
        result
    }

    /// Set an event: wake everyone waiting for it (in arrival order) and publish it for
    /// `publish_for`. Not idempotent: every call is a new publication with a new id.
    pub fn event_set(&mut self, now: Time, req: EventSetRequest) -> Result<SetOutcome, Error> {
        self.advance(now);
        let now = self.now;
        self.validate_event_name(&req.name)?;
        if req
            .value
            .as_ref()
            .is_some_and(|v| v.len() > self.cfg.max_event_value_len)
        {
            return Err(Error::InvalidArgument("event value is too long"));
        }
        let publish_for = req
            .publish_for
            .unwrap_or(self.cfg.event_default_publish)
            .min(self.cfg.event_max_publish);

        let id = self.next_token;
        self.next_token += 1;
        let (name, mut state) = self
            .event_table
            .remove_entry(&req.name)
            .unwrap_or_else(|| (req.name.clone(), EventState::default()));

        let waiters = std::mem::take(&mut state.waiters);
        let woken = waiters.len();
        for w in waiters {
            self.forget_event(w.session, w.request);
            self.events.push(Event::Published {
                session: w.session,
                request: w.request,
                name: name.clone(),
                publication: Publication {
                    id,
                    value: req.value.clone(),
                    age: Duration::ZERO,
                },
            });
        }
        // The latest set wins: it replaces whatever was published before, even with a zero window.
        state.published = (!publish_for.is_zero()).then(|| Published {
            id,
            value: req.value.clone(),
            set_at: now,
            until: now + publish_for,
        });
        self.put_event(name, state);
        Ok(SetOutcome {
            woken,
            id,
            publish_for,
        })
    }

    fn cancel_event_wait(&mut self, session: SessionId, request: RequestId) -> CancelOutcome {
        let Some(name) = self
            .sessions
            .get(&session)
            .and_then(|s| s.events.get(&request))
            .cloned()
        else {
            return CancelOutcome::NotFound;
        };
        let Some((name, mut state)) = self.event_table.remove_entry(&name) else {
            return CancelOutcome::NotFound;
        };
        let outcome = match state
            .waiters
            .iter()
            .position(|w| w.session == session && w.request == request)
        {
            Some(pos) => {
                state.waiters.remove(pos);
                CancelOutcome::Dequeued
            }
            None => CancelOutcome::NotFound,
        };
        self.forget_event(session, request);
        self.put_event(name, state);
        outcome
    }

    fn validate_event_name(&self, name: &str) -> Result<(), Error> {
        if name.is_empty() || name.len() > self.cfg.max_name_len {
            return Err(Error::InvalidArgument("name length out of range"));
        }
        Ok(())
    }

    fn on_event_wait_timeout(&mut self, name: Name, session: SessionId, request: RequestId) {
        let now = self.now;
        let Some(mut state) = self.event_table.remove(&name) else {
            return;
        };
        // `deadline <= now` guards against a stale timer of an earlier incarnation of this request id.
        if let Some(pos) = state
            .waiters
            .iter()
            .position(|w| w.session == session && w.request == request && w.deadline <= now)
        {
            state.waiters.remove(pos);
            self.forget_event(session, request);
            self.events.push(Event::WaitTimedOut {
                session,
                request,
                name: name.clone(),
            });
        }
        self.put_event(name, state);
    }

    fn on_event_timer(&mut self, name: Name) {
        let Some(mut state) = self.event_table.remove(&name) else {
            return;
        };
        if state.timer_at.is_some_and(|at| at <= self.now) {
            state.timer_at = None;
        }
        self.put_event(name, state);
    }

    /// Return an event name to the table, or drop it if nobody waits and nothing is published.
    /// A live publication keeps an expiry timer, so the name is collected when its window ends.
    fn put_event(&mut self, name: Name, mut state: EventState) {
        let now = self.now;
        if state.published.as_ref().is_some_and(|p| p.until <= now) {
            state.published = None;
        }
        if let Some(until) = state.published.as_ref().map(|p| p.until)
            && state.timer_at.is_none_or(|scheduled| scheduled > until)
        {
            state.timer_at = Some(until);
            self.schedule(until, Timer::EventExpire { name: name.clone() });
        }
        if state.waiters.is_empty() && state.published.is_none() {
            return; // nothing to remember: the name leaves the table
        }
        self.event_table.insert(name, state);
    }

    /// Verify every structural invariant. O(everything); for tests and debug assertions.
    #[doc(hidden)]
    pub fn check_invariants(&self) -> Result<(), String> {
        macro_rules! ensure {
            ($cond:expr, $($arg:tt)+) => {
                if !$cond {
                    return Err(format!($($arg)+));
                }
            };
        }

        let now = self.now;
        let mut expire_timers: HashMap<(Name, FencingToken), Time> = HashMap::new();
        let mut wait_timers: HashMap<(Name, SessionId, RequestId), Vec<Time>> = HashMap::new();
        let mut event_wait_timers: HashMap<(Name, SessionId, RequestId), Vec<Time>> =
            HashMap::new();
        let mut event_expiry: HashMap<Name, Time> = HashMap::new();
        for ((at, _), timer) in &self.timers {
            match timer {
                Timer::Expire { name, token } => {
                    let slot = expire_timers.entry((name.clone(), *token)).or_insert(*at);
                    *slot = (*slot).min(*at);
                }
                Timer::Wait {
                    name,
                    session,
                    request,
                } => {
                    wait_timers
                        .entry((name.clone(), *session, *request))
                        .or_default()
                        .push(*at);
                }
                Timer::Entry { .. } => {}
                Timer::EventWait {
                    name,
                    session,
                    request,
                } => {
                    event_wait_timers
                        .entry((name.clone(), *session, *request))
                        .or_default()
                        .push(*at);
                }
                Timer::EventExpire { name } => {
                    let slot = event_expiry.entry(name.clone()).or_insert(*at);
                    *slot = (*slot).min(*at);
                }
            }
        }

        let mut tokens = HashSet::new();
        let mut live = 0usize;
        for (name, entry) in &self.entries {
            match entry.kind {
                Kind::Mutex => {
                    ensure!(
                        entry.holders.len() <= 1,
                        "{name}: mutex held {} times",
                        entry.holders.len()
                    );
                    ensure!(
                        entry.holders.iter().all(|h| h.want == Want::Mutex),
                        "{name}: non-mutex holder"
                    );
                }
                Kind::RwLock => {
                    let writers = entry
                        .holders
                        .iter()
                        .filter(|h| h.want == Want::Write)
                        .count();
                    let readers = entry
                        .holders
                        .iter()
                        .filter(|h| h.want == Want::Read)
                        .count();
                    ensure!(
                        writers + readers == entry.holders.len(),
                        "{name}: foreign holder in rwlock"
                    );
                    ensure!(
                        writers <= 1 && (writers == 0 || readers == 0),
                        "{name}: {writers} writers with {readers} readers"
                    );
                }
                Kind::Semaphore { capacity } => {
                    ensure!(
                        entry.holders.iter().all(
                            |h| matches!(h.want, Want::Semaphore { capacity: c, .. } if c == capacity)
                        ),
                        "{name}: foreign holder in semaphore"
                    );
                    ensure!(
                        entry.used_permits() <= u64::from(capacity),
                        "{name}: {} permits used of {capacity}",
                        entry.used_permits()
                    );
                }
            }

            for h in &entry.holders {
                ensure!(
                    tokens.insert(h.token),
                    "{name}: duplicate token {:?}",
                    h.token
                );
                ensure!(h.token.0 < self.next_token, "{name}: token from the future");
                ensure!(
                    h.expires_at > now,
                    "{name}: holder {:?} is past its expiry",
                    h.token
                );
                ensure!(
                    self.sessions
                        .get(&h.session)
                        .and_then(|s| s.requests.get(&h.request))
                        .is_some_and(|n| n == name),
                    "{name}: holder {:?} not indexed",
                    h.token
                );
                ensure!(
                    expire_timers
                        .get(&(name.clone(), h.token))
                        .is_some_and(|at| *at <= h.expires_at),
                    "{name}: holder {:?} has no expiry timer",
                    h.token
                );
                live += 1;
            }
            for w in &entry.queue {
                ensure!(
                    w.want.kind() == entry.kind,
                    "{name}: waiter of a different kind"
                );
                ensure!(w.deadline > now, "{name}: waiter past its deadline");
                ensure!(
                    self.sessions
                        .get(&w.session)
                        .and_then(|s| s.requests.get(&w.request))
                        .is_some_and(|n| n == name),
                    "{name}: waiter not indexed"
                );
                ensure!(
                    wait_timers
                        .get(&(name.clone(), w.session, w.request))
                        .is_some_and(|ats| ats.contains(&w.deadline)),
                    "{name}: waiter has no timeout timer"
                );
                live += 1;
            }

            if entry.delay_until <= now
                && let Some(head) = entry.queue.front()
            {
                ensure!(
                    !entry.can_admit(head.want),
                    "{name}: missed wakeup for queue head"
                );
            }
            if entry.holders.is_empty() && entry.queue.is_empty() {
                let residue =
                    entry.state.is_some_and(|s| s.expires_at > now) || entry.delay_until > now;
                ensure!(
                    residue,
                    "{name}: idle entry with no residue should have been dropped"
                );
            }
        }

        let mut event_live = 0usize;
        for (name, state) in &self.event_table {
            ensure!(
                !state.waiters.is_empty() || state.published.is_some(),
                "{name}: idle event kept with nothing waiting or published"
            );
            for w in &state.waiters {
                ensure!(w.deadline > now, "{name}: event waiter past its deadline");
                ensure!(
                    self.sessions
                        .get(&w.session)
                        .and_then(|s| s.events.get(&w.request))
                        .is_some_and(|n| n == name),
                    "{name}: event waiter not indexed"
                );
                ensure!(
                    event_wait_timers
                        .get(&(name.clone(), w.session, w.request))
                        .is_some_and(|ats| ats.contains(&w.deadline)),
                    "{name}: event waiter has no timeout timer"
                );
                event_live += 1;
            }
            if let Some(p) = &state.published {
                ensure!(p.until > now, "{name}: publication outlived its window");
                ensure!(
                    p.id < self.next_token,
                    "{name}: publication id from the future"
                );
                ensure!(
                    event_expiry.get(name).is_some_and(|at| *at <= p.until),
                    "{name}: publication has no expiry timer"
                );
            }
        }

        let mut indexed = 0usize;
        let mut event_indexed = 0usize;
        for (session, index) in &self.sessions {
            ensure!(!index.is_empty(), "{session:?}: empty session index kept");
            for request in index.events.keys() {
                ensure!(
                    !index.requests.contains_key(request),
                    "{session:?}/{request:?}: used by a lock request and an event wait"
                );
            }
            for (request, name) in &index.events {
                let waiting = self.event_table.get(name).is_some_and(|e| {
                    e.waiters
                        .iter()
                        .any(|w| w.session == *session && w.request == *request)
                });
                ensure!(
                    waiting,
                    "{session:?}/{request:?}: indexed under {name} but not waiting there"
                );
                event_indexed += 1;
            }
            for (request, name) in &index.requests {
                let Some(entry) = self.entries.get(name) else {
                    return Err(format!(
                        "{session:?}/{request:?}: indexed under missing entry {name}"
                    ));
                };
                let held = entry
                    .holders
                    .iter()
                    .any(|h| h.session == *session && h.request == *request);
                let queued = entry
                    .queue
                    .iter()
                    .any(|w| w.session == *session && w.request == *request);
                ensure!(
                    held ^ queued,
                    "{session:?}/{request:?}: held={held} queued={queued}"
                );
                indexed += 1;
            }
        }
        ensure!(
            indexed == live,
            "index has {indexed} requests but {live} are live"
        );
        ensure!(
            event_indexed == event_live,
            "index has {event_indexed} event waits but {event_live} are waiting"
        );
        Ok(())
    }
}
