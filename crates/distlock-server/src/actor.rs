//! The lock engine: one task that owns the `LockTable` and the session registry.
//!
//! Connection tasks never touch lock state. They send `Command`s here and receive `ServerFrame`s
//! back on a bounded per-connection queue. Because one task owns everything, there are no locks
//! around lock state and every operation is serialised in a single well-defined order.
//!
//! Delivery never blocks the engine: if a connection's outbound queue is full (a client that
//! stopped reading) the connection is killed instead of being waited for.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use distlock_core::{
    AcquireRequest, AcquireStatus, CancelOutcome, Event, FencingToken, LockTable, Name, RequestId,
    SessionId,
};
use distlock_proto::v1 as pb;
use distlock_proto::v1::{client_frame, server_frame};
use subtle::ConstantTimeEq;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::config::ServerConfig;
use crate::convert::{
    core_error_to_pb, dur, error_body, frame, grant_to_pb, ms, release_state_from_pb, view_to_pb,
    want_from_pb,
};
use crate::users::{Reload, UserDb};
use crate::util::random_bytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ConnId(pub u64);

pub(crate) enum Outbound {
    Frame(pb::ServerFrame),
    /// Flush what is queued, then close the connection.
    Close,
}

/// The engine's handle on one connection.
#[derive(Clone)]
pub(crate) struct ConnHandle {
    pub id: ConnId,
    pub tx: mpsc::Sender<Outbound>,
    /// Cancelled by the engine to force the connection down (slow consumer).
    pub kill: CancellationToken,
}

pub(crate) enum Command {
    /// Credentials were verified: create a session and make this its first connection.
    Open {
        conn: ConnHandle,
        seq: u64,
        client_id: String,
        label: String,
    },
    /// Join an existing session (pool connection or reconnect). Replies with the session view.
    Attach {
        conn: ConnHandle,
        seq: u64,
        session_id: u64,
        token: Vec<u8>,
    },
    /// A post-handshake request from an attached connection.
    Request {
        conn: ConnId,
        seq: u64,
        body: client_frame::Body,
    },
    /// The users file was reloaded (by `ResetUsers` from `conn`): throw out whoever lost access.
    UsersReset {
        conn: ConnId,
        seq: u64,
        reload: Reload,
    },
    ConnClosed {
        conn: ConnId,
    },
    Shutdown {
        drain: Duration,
    },
}

struct Session {
    client_id: String,
    label: String,
    token: [u8; 32],
    /// Oldest first. Pushes prefer the newest connection.
    conns: Vec<ConnId>,
    /// Since when the session has had no connection.
    idle_since: Option<Instant>,
}

struct ConnState {
    tx: mpsc::Sender<Outbound>,
    kill: CancellationToken,
    session: SessionId,
}

/// Session ids in logs.
struct Hex(u64);
impl fmt::Display for Hex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

pub(crate) struct Actor {
    cfg: Arc<ServerConfig>,
    table: LockTable,
    /// The table's time zero. Table time is `start.elapsed()`.
    start: Instant,
    rx: mpsc::Receiver<Command>,
    /// Consulted (lock-free) when a session is created or joined, to close the window between a
    /// password check and the engine seeing the login.
    users: Arc<UserDb>,
    sessions: HashMap<SessionId, Session>,
    conns: HashMap<ConnId, ConnState>,
}

impl Actor {
    pub(crate) fn new(
        cfg: Arc<ServerConfig>,
        table: LockTable,
        rx: mpsc::Receiver<Command>,
        users: Arc<UserDb>,
    ) -> Self {
        Self {
            cfg,
            table,
            start: Instant::now(),
            rx,
            users,
            sessions: HashMap::new(),
            conns: HashMap::new(),
        }
    }

    pub(crate) async fn run(mut self) {
        let mut housekeeping = tokio::time::interval(Duration::from_millis(500));
        housekeeping.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            // Sleep until the table's next timer (lease expiry, wait timeout, lock-delay, GC).
            let wake = match self.table.next_deadline() {
                Some(at) => self.start + at,
                None => Instant::now() + Duration::from_secs(3600),
            };
            tokio::select! {
                cmd = self.rx.recv() => match cmd {
                    None => break,
                    Some(Command::Shutdown { drain }) => {
                        self.broadcast_shutdown(drain);
                        tokio::time::sleep(drain).await;
                        self.close_all_connections();
                        break;
                    }
                    Some(cmd) => self.handle(cmd),
                },
                () = tokio::time::sleep_until(wake) => self.tick(),
                _ = housekeeping.tick() => {
                    self.tick();
                    self.reap_sessions();
                }
            }
            self.flush_events();
        }
        info!("lock engine stopped");
    }

    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn tick(&mut self) {
        let now = self.now();
        self.table.poll(now);
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Open {
                conn,
                seq,
                client_id,
                label,
            } => self.open(conn, seq, client_id, label),
            Command::Attach {
                conn,
                seq,
                session_id,
                token,
            } => self.attach(conn, seq, session_id, &token),
            Command::Request { conn, seq, body } => self.request(conn, seq, body),
            Command::UsersReset { conn, seq, reload } => self.users_reset(conn, seq, reload),
            Command::ConnClosed { conn } => self.remove_conn(conn),
            Command::Shutdown { .. } => {} // handled in `run`
        }
    }

    // ------------------------------------------------------------------ sessions

    fn open(&mut self, conn: ConnHandle, seq: u64, client_id: String, label: String) {
        if !self.users.is_enabled(&client_id) {
            // The password was right when it was checked, but a reload removed the user since.
            warn!(client_id = %client_id, "login refused: the user was removed while logging in");
            let _ = conn.tx.try_send(Outbound::Frame(frame(
                seq,
                error_body(pb::ErrorCode::Unauthenticated, "invalid credentials"),
            )));
            let _ = conn.tx.try_send(Outbound::Close);
            return;
        }
        let sid = self.new_session_id();
        let token = random_bytes::<32>();
        info!(session = %Hex(sid.0), client_id = %client_id, label = ?label, "session opened");
        self.sessions.insert(
            sid,
            Session {
                client_id,
                label,
                token,
                conns: vec![conn.id],
                idle_since: None,
            },
        );
        self.conns.insert(
            conn.id,
            ConnState {
                tx: conn.tx,
                kill: conn.kill,
                session: sid,
            },
        );
        let ok = pb::HelloOk {
            session_id: sid.0,
            session_token: token.to_vec(),
            max_frame_bytes: u32::try_from(self.cfg.max_frame_bytes).unwrap_or(u32::MAX),
            max_ttl_ms: ms(self.cfg.lock.max_ttl),
            max_wait_ms: ms(self.cfg.lock.max_wait),
            ping_interval_ms: ms(self.cfg.ping_interval),
        };
        self.reply(conn.id, seq, server_frame::Body::HelloOk(ok));
    }

    fn attach(&mut self, conn: ConnHandle, seq: u64, session_id: u64, token: &[u8]) {
        let sid = SessionId(session_id);
        let failure = match self.sessions.get(&sid) {
            None => Some((
                pb::ErrorCode::SessionNotFound,
                "session does not exist (expired, closed, or the server restarted): all its locks are lost",
            )),
            Some(s) if !bool::from(s.token.as_slice().ct_eq(token)) => {
                Some((pb::ErrorCode::Unauthenticated, "invalid session token"))
            }
            Some(s) if !self.users.is_enabled(&s.client_id) => {
                Some((pb::ErrorCode::Unauthenticated, "invalid credentials"))
            }
            Some(s) if s.conns.len() >= self.cfg.max_connections_per_session => Some((
                pb::ErrorCode::ResourceExhausted,
                "too many connections attached to this session",
            )),
            Some(_) => None,
        };
        if let Some((code, message)) = failure {
            warn!(session = %Hex(session_id), code = ?code, "attach refused");
            // The connection is not registered, so answer on its queue directly, then close.
            let _ = conn
                .tx
                .try_send(Outbound::Frame(frame(seq, error_body(code, message))));
            let _ = conn.tx.try_send(Outbound::Close);
            return;
        }
        if let Some(sess) = self.sessions.get_mut(&sid) {
            sess.conns.push(conn.id);
            sess.idle_since = None;
            debug!(session = %Hex(session_id), client_id = %sess.client_id, conns = sess.conns.len(), "connection attached");
        }
        self.conns.insert(
            conn.id,
            ConnState {
                tx: conn.tx,
                kill: conn.kill,
                session: sid,
            },
        );
        let now = self.now();
        let view = self.table.session_view(now, sid);
        let ok = pb::AttachOk {
            view: Some(view_to_pb(view)),
            max_frame_bytes: u32::try_from(self.cfg.max_frame_bytes).unwrap_or(u32::MAX),
            ping_interval_ms: ms(self.cfg.ping_interval),
        };
        self.reply(conn.id, seq, server_frame::Body::AttachOk(ok));
    }

    fn new_session_id(&self) -> SessionId {
        loop {
            let id = u64::from_le_bytes(random_bytes::<8>());
            if id != 0 && !self.sessions.contains_key(&SessionId(id)) {
                return SessionId(id);
            }
        }
    }

    fn remove_conn(&mut self, conn: ConnId) {
        let Some(state) = self.conns.remove(&conn) else {
            return;
        };
        if let Some(sess) = self.sessions.get_mut(&state.session) {
            sess.conns.retain(|c| *c != conn);
            if sess.conns.is_empty() {
                sess.idle_since = Some(Instant::now());
                debug!(session = %Hex(state.session.0), "session has no connections; locks stay held until their leases expire");
            }
        }
    }

    /// Forget sessions that have been disconnected for the idle timeout and hold or await nothing.
    fn reap_sessions(&mut self) {
        let timeout = self.cfg.session_idle_timeout;
        let table = &self.table;
        let victims: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(sid, s)| {
                s.conns.is_empty()
                    && s.idle_since.is_some_and(|t| t.elapsed() >= timeout)
                    && !table.session_has_requests(**sid)
            })
            .map(|(sid, _)| *sid)
            .collect();
        for sid in victims {
            if let Some(s) = self.sessions.remove(&sid) {
                info!(session = %Hex(sid.0), client_id = %s.client_id, label = ?s.label, "session expired (idle, no locks)");
            }
        }
    }

    // ------------------------------------------------------------------ requests

    fn request(&mut self, conn: ConnId, seq: u64, body: client_frame::Body) {
        let Some(sid) = self.conns.get(&conn).map(|c| c.session) else {
            return; // connection was already dropped
        };
        let now = self.now();
        match body {
            client_frame::Body::Acquire(a) => self.acquire(conn, seq, sid, now, a),
            client_frame::Body::CancelAcquire(c) => {
                let outcome = match self.table.cancel(now, sid, RequestId(c.request_id)) {
                    CancelOutcome::NotFound => pb::cancel_ok::Outcome::NotFound,
                    CancelOutcome::Dequeued => pb::cancel_ok::Outcome::Dequeued,
                    CancelOutcome::ReleasedGrant => pb::cancel_ok::Outcome::ReleasedGrant,
                };
                self.reply(
                    conn,
                    seq,
                    server_frame::Body::CancelOk(pb::CancelOk {
                        outcome: outcome as i32,
                    }),
                );
            }
            client_frame::Body::Release(r) => {
                let state = match release_state_from_pb(r.state) {
                    Ok(s) => s,
                    Err(msg) => {
                        return self.reply(
                            conn,
                            seq,
                            error_body(pb::ErrorCode::InvalidArgument, msg),
                        );
                    }
                };
                let state_ttl = (r.state_ttl_ms != 0).then(|| dur(r.state_ttl_ms));
                match self.table.release(
                    now,
                    sid,
                    &r.name,
                    FencingToken(r.fencing_token),
                    state,
                    state_ttl,
                ) {
                    Ok(()) => {
                        self.reply(conn, seq, server_frame::Body::ReleaseOk(pb::ReleaseOk {}))
                    }
                    Err(e) => self.reply_core_error(conn, seq, &e),
                }
            }
            client_frame::Body::Extend(e) => {
                let results: Vec<pb::ExtendResult> = e
                    .items
                    .into_iter()
                    .map(|item| {
                        let res = self.table.extend(
                            now,
                            sid,
                            &item.name,
                            FencingToken(item.fencing_token),
                            dur(item.ttl_ms),
                        );
                        let (code, remaining) = match res {
                            Ok(remaining) => (pb::ErrorCode::Ok, remaining),
                            Err(distlock_core::Error::NotHeld) => {
                                (pb::ErrorCode::NotHeld, Duration::ZERO)
                            }
                            Err(distlock_core::Error::InvalidArgument(_)) => {
                                (pb::ErrorCode::InvalidArgument, Duration::ZERO)
                            }
                            Err(other) => {
                                error!(error = %other, "unexpected error extending a lease");
                                (pb::ErrorCode::Internal, Duration::ZERO)
                            }
                        };
                        pb::ExtendResult {
                            name: item.name,
                            fencing_token: item.fencing_token,
                            code: code as i32,
                            ttl_remaining_ms: ms(remaining),
                        }
                    })
                    .collect();
                self.reply(
                    conn,
                    seq,
                    server_frame::Body::ExtendOk(pb::ExtendOk { results }),
                );
            }
            client_frame::Body::CloseSession(_) => self.close_session(conn, seq, sid, now),
            client_frame::Body::Hello(_)
            | client_frame::Body::Attach(_)
            | client_frame::Body::Ping(_)
            | client_frame::Body::Maintenance(_) => {
                self.reply(
                    conn,
                    seq,
                    error_body(pb::ErrorCode::InvalidArgument, "unexpected message"),
                );
            }
        }
    }

    fn acquire(&mut self, conn: ConnId, seq: u64, sid: SessionId, now: Duration, a: pb::Acquire) {
        let want = match want_from_pb(a.mode, a.semaphore_capacity, a.permits) {
            Ok(w) => w,
            Err(msg) => {
                return self.reply(conn, seq, error_body(pb::ErrorCode::InvalidArgument, msg));
            }
        };
        let name: Name = Arc::from(a.name.as_str());
        let request = RequestId(a.request_id);
        let req = AcquireRequest {
            session: sid,
            request,
            name: name.clone(),
            want,
            ttl: dur(a.ttl_ms),
            wait: dur(a.wait_ms),
        };
        match self.table.acquire(now, req) {
            Ok(AcquireStatus::Granted(grant)) => {
                let outcome = pb::acquire_ok::Outcome::Granted(grant_to_pb(request, &name, &grant));
                self.reply(
                    conn,
                    seq,
                    server_frame::Body::AcquireOk(pb::AcquireOk {
                        outcome: Some(outcome),
                    }),
                );
            }
            Ok(AcquireStatus::Queued) => {
                let outcome = pb::acquire_ok::Outcome::Queued(pb::Queued {});
                self.reply(
                    conn,
                    seq,
                    server_frame::Body::AcquireOk(pb::AcquireOk {
                        outcome: Some(outcome),
                    }),
                );
            }
            Err(e) => self.reply_core_error(conn, seq, &e),
        }
    }

    /// Graceful end: release everything, answer, then close every connection of the session.
    fn close_session(&mut self, conn: ConnId, seq: u64, sid: SessionId, now: Duration) {
        self.table.close_session(now, sid);
        self.reply(
            conn,
            seq,
            server_frame::Body::CloseSessionOk(pb::CloseSessionOk {}),
        );
        if let Some(sess) = self.sessions.remove(&sid) {
            info!(session = %Hex(sid.0), client_id = %sess.client_id, "session closed by client");
            for c in sess.conns {
                if let Some(state) = self.conns.remove(&c)
                    && state.tx.try_send(Outbound::Close).is_err()
                {
                    state.kill.cancel();
                }
            }
        }
    }

    /// New users are in force. End the sessions of everyone who was removed or disabled (their locks
    /// are released as after a lost lease, their waiting requests cancelled) and close all their TCP
    /// connections. The sender gets its report *first*, even if its own connection is among those
    /// about to be closed, so it learns what happened to it.
    fn users_reset(&mut self, conn: ConnId, seq: u64, reload: Reload) {
        let revoked = reload.revoked();
        let invoker_session = self.conns.get(&conn).map(|c| c.session);
        let victims: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(_, s)| revoked.contains(&s.client_id))
            .map(|(sid, _)| *sid)
            .collect();
        let now = self.now();
        let mut doomed: Vec<ConnId> = Vec::new();
        let mut invoker_dropped = false;
        for sid in &victims {
            self.table.revoke_session(now, *sid);
            if let Some(sess) = self.sessions.remove(sid) {
                warn!(
                    session = %Hex(sid.0),
                    client_id = %sess.client_id,
                    label = ?sess.label,
                    connections = sess.conns.len(),
                    "session ended: the user was removed or disabled"
                );
                invoker_dropped |= Some(*sid) == invoker_session;
                doomed.extend(sess.conns);
            }
        }
        if !reload.unchanged {
            info!(
                generation = reload.generation,
                added = ?reload.added,
                removed = ?reload.removed,
                disabled = ?reload.disabled,
                enabled = ?reload.enabled,
                password_changed = ?reload.password_changed,
                sessions_ended = victims.len(),
                "users reloaded"
            );
        }
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        let report = pb::ResetUsersOk {
            added: count(reload.added.len()),
            removed: count(reload.removed.len()),
            disabled: count(reload.disabled.len()),
            enabled: count(reload.enabled.len()),
            password_changed: count(reload.password_changed.len()),
            sessions_closed: count(victims.len()),
            connections_closed: count(doomed.len()),
            invoker_dropped,
        };
        // Reply first: the close below is queued behind it on the same connection.
        self.reply(
            conn,
            seq,
            server_frame::Body::MaintenanceOk(pb::MaintenanceOk {
                outcome: Some(pb::maintenance_ok::Outcome::ResetUsers(report)),
            }),
        );
        for c in doomed {
            if let Some(state) = self.conns.remove(&c)
                && state.tx.try_send(Outbound::Close).is_err()
            {
                state.kill.cancel();
            }
        }
    }

    // ------------------------------------------------------------------ output

    /// Turn the table's asynchronous events into pushes on the owning session.
    fn flush_events(&mut self) {
        for event in self.table.take_events() {
            match event {
                Event::Granted {
                    session,
                    request,
                    name,
                    grant,
                } => {
                    let grant = grant_to_pb(request, &name, &grant);
                    self.push(
                        session,
                        server_frame::Body::Granted(pb::Granted { grant: Some(grant) }),
                    );
                }
                Event::WaitTimedOut {
                    session,
                    request,
                    name,
                } => {
                    let body = pb::WaitTimedOut {
                        request_id: request.0,
                        name: name.to_string(),
                    };
                    self.push(session, server_frame::Body::WaitTimedOut(body));
                }
                Event::LeaseExpired {
                    session,
                    name,
                    token,
                } => {
                    warn!(session = %Hex(session.0), lock = %name, token = token.0, "lease expired");
                    let body = pb::LeaseExpired {
                        name: name.to_string(),
                        fencing_token: token.0,
                    };
                    self.push(session, server_frame::Body::LeaseExpired(body));
                }
            }
        }
    }

    /// Deliver an unsolicited message on *any* connection of the session (newest first). If the
    /// session has none, drop it: the client learns the truth from the view returned by `Attach`.
    fn push(&mut self, sid: SessionId, body: server_frame::Body) {
        let Some(sess) = self.sessions.get(&sid) else {
            return;
        };
        let candidates: Vec<ConnId> = sess.conns.iter().rev().copied().collect();
        let message = frame(0, body);
        for conn in candidates {
            if self.deliver(conn, Outbound::Frame(message.clone())) {
                return;
            }
        }
        debug!(session = %Hex(sid.0), "push dropped: no connection attached");
    }

    fn reply(&mut self, conn: ConnId, seq: u64, body: server_frame::Body) {
        self.deliver(conn, Outbound::Frame(frame(seq, body)));
    }

    fn reply_core_error(&mut self, conn: ConnId, seq: u64, err: &distlock_core::Error) {
        if let distlock_core::Error::Internal(what) = err {
            error!(what, "lock table inconsistency");
        }
        self.reply(conn, seq, server_frame::Body::Error(core_error_to_pb(err)));
    }

    /// Never blocks. A connection that cannot take the message is killed and forgotten.
    fn deliver(&mut self, conn: ConnId, out: Outbound) -> bool {
        let Some(state) = self.conns.get(&conn) else {
            return false;
        };
        match state.tx.try_send(out) {
            Ok(()) => true,
            Err(e) => {
                warn!(conn = conn.0, reason = %e, "cannot queue message for connection; closing it");
                state.kill.cancel();
                self.remove_conn(conn);
                false
            }
        }
    }

    /// Close every connection after flushing what is queued. Connection tasks keep their own
    /// queue handles, so merely dropping ours would leave them open.
    fn close_all_connections(&mut self) {
        for (_, state) in self.conns.drain() {
            if state.tx.try_send(Outbound::Close).is_err() {
                state.kill.cancel();
            }
        }
    }

    fn broadcast_shutdown(&mut self, drain: Duration) {
        info!(
            connections = self.conns.len(),
            "shutting down; notifying clients"
        );
        let message = frame(
            0,
            server_frame::Body::Shutdown(pb::Shutdown {
                drain_ms: ms(drain),
            }),
        );
        for state in self.conns.values() {
            let _ = state.tx.try_send(Outbound::Frame(message.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    //! The engine's own guard against a user who loses access between the password check (in the
    //! connection task) and the engine seeing the login. That window cannot be hit reliably over a
    //! socket, so these drive the engine with commands directly.

    use super::*;
    use distlock_core::Config as LockConfig;

    struct Wire {
        conn: ConnHandle,
        rx: mpsc::Receiver<Outbound>,
    }

    fn wire(id: u64) -> Wire {
        let (tx, rx) = mpsc::channel(16);
        Wire {
            conn: ConnHandle {
                id: ConnId(id),
                tx,
                kill: CancellationToken::new(),
            },
            rx,
        }
    }

    async fn next(rx: &mut mpsc::Receiver<Outbound>) -> Outbound {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the engine answered")
            .expect("the channel is open")
    }

    fn expect_error(out: Outbound, code: pb::ErrorCode) {
        match out {
            Outbound::Frame(f) => match f.body {
                Some(server_frame::Body::Error(e)) => assert_eq!(e.code, code as i32, "{e:?}"),
                other => panic!("expected an error, got {other:?}"),
            },
            Outbound::Close => panic!("expected an error frame, got a close"),
        }
    }

    fn start(users: UserDb) -> (mpsc::Sender<Command>, Arc<UserDb>) {
        let cfg = Arc::new(ServerConfig::default());
        let users = Arc::new(users);
        let (tx, rx) = mpsc::channel(16);
        let actor = Actor::new(
            Arc::clone(&cfg),
            LockTable::new(LockConfig::default()),
            rx,
            Arc::clone(&users),
        );
        tokio::spawn(actor.run());
        (tx, users)
    }

    #[tokio::test]
    async fn a_login_for_a_user_that_is_no_longer_allowed_is_refused_even_if_the_password_was_right()
     {
        let (engine, _) = start(UserDb::for_tests(&[
            ("alive", "pw", true),
            ("ghost", "pw", false),
        ]));

        let mut ok = wire(1);
        engine
            .send(Command::Open {
                conn: ok.conn.clone(),
                seq: 1,
                client_id: "alive".into(),
                label: String::new(),
            })
            .await
            .unwrap();
        assert!(matches!(
            next(&mut ok.rx).await,
            Outbound::Frame(f) if matches!(f.body, Some(server_frame::Body::HelloOk(_)))
        ));

        // The connection task verified "ghost" a moment ago; by the time the engine sees the login,
        // the users in force say it may not log in.
        let mut ghost = wire(2);
        engine
            .send(Command::Open {
                conn: ghost.conn.clone(),
                seq: 1,
                client_id: "ghost".into(),
                label: String::new(),
            })
            .await
            .unwrap();
        expect_error(next(&mut ghost.rx).await, pb::ErrorCode::Unauthenticated);
        assert!(matches!(next(&mut ghost.rx).await, Outbound::Close));
    }

    #[tokio::test]
    async fn a_session_cannot_be_joined_once_its_user_has_lost_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");
        let hash = bcrypt::hash("pw", 10).unwrap();
        let write = |enabled: bool| {
            let doc = serde_json::json!({"version": 1, "users": [
                {"client_id": "app", "password_hash": hash, "enabled": enabled}
            ]});
            std::fs::write(&path, doc.to_string()).unwrap();
        };
        write(true);
        let (engine, users) = start(UserDb::load(&path, 1).unwrap());

        let mut first = wire(1);
        engine
            .send(Command::Open {
                conn: first.conn.clone(),
                seq: 1,
                client_id: "app".into(),
                label: String::new(),
            })
            .await
            .unwrap();
        let Outbound::Frame(f) = next(&mut first.rx).await else {
            panic!("expected HelloOk")
        };
        let Some(server_frame::Body::HelloOk(hello)) = f.body else {
            panic!("expected HelloOk")
        };

        // The users change, but the engine has not been told to end sessions (as if the reload and
        // the revoke were still racing): the session exists, yet its user may no longer log in.
        write(false);
        users.reload().await.unwrap();

        let mut second = wire(2);
        engine
            .send(Command::Attach {
                conn: second.conn.clone(),
                seq: 1,
                session_id: hello.session_id,
                token: hello.session_token.clone(),
            })
            .await
            .unwrap();
        expect_error_code_and_close(&mut second.rx).await;
    }

    async fn expect_error_code_and_close(rx: &mut mpsc::Receiver<Outbound>) {
        expect_error(next(rx).await, pb::ErrorCode::Unauthenticated);
        assert!(matches!(next(rx).await, Outbound::Close));
    }
}
