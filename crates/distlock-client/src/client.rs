//! The pool, the session, and everything that has to keep working while connections come and go.
//!
//! One `Client` owns one *session* on the server, shared by `pool_size` TCP connections. Each
//! connection slot has a supervisor task that keeps it connected: the first connection logs in
//! (`Hello`, bcrypt) and creates the session; every other connection, and every reconnect, joins it
//! (`Attach`) and reconciles local state against the server's view of the session.
//!
//! Locks never belong to a connection, only to the session, so any request can use any connection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use distlock_proto::v1 as pb;
use distlock_proto::v1::{client_frame as cf, server_frame as sf};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio::net::TcpStream;
use tokio::runtime::Handle;
use tokio::sync::{Notify, oneshot, watch};
use tokio::time::Instant;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::conn::{Conn, Io, REQUEST_TIMEOUT, Reader, Writer, tune_socket};
use crate::error::{Error, Result};
use crate::event::{EventSet, EventWait, Published, SetResult};
use crate::lock::{Acquire, Kind, Lock, Previous, State, family_name, wire_name};
use crate::maintenance::{Maintenance, protocol};
use crate::tls::{self, Tls};

const PROTOCOL_VERSION: u32 = 1;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a call waits for a usable connection before giving up with `NotConnected`.
const WAIT_FOR_CONNECTION: Duration = Duration::from_secs(10);
/// Extra time, beyond the server-side wait, before a silent server is treated as having lost a push.
const WAIT_SLACK: Duration = Duration::from_secs(5);
/// Retry budget for `Unavailable` (the server's startup grace period) when the caller set no wait.
const UNAVAILABLE_BUDGET: Duration = Duration::from_secs(120);
const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_RETRIES: usize = 3;
/// A connection that lived at least this long counts as healthy: when it ends, reconnect at once.
/// One that dies sooner is a flapping connection and is retried with growing backoff instead.
const STABLE_AFTER: Duration = Duration::from_secs(5);
/// A maintenance command may do real work on the server (re-reading and re-hashing).
const MAINTENANCE_TIMEOUT: Duration = Duration::from_secs(60);

// ------------------------------------------------------------------------------------ public config

/// Connection settings. Build one, then [`connect`](DistLockClient::connect) (async) or
/// `connect_blocking`.
///
/// ```no_run
/// # use std::time::Duration;
/// # async fn demo() -> distlock_client::Result<()> {
/// use distlock_client::DistLockClient;
/// let client = DistLockClient {
///     client_id: "billing-service".into(),
///     password: "secret".into(),
///     host: "10.0.0.5".into(),
///     port: 7878,
///     namespace: Some("billing".into()),
///     pool_size: 4,
///     tls: false,
///     tls_verify: true,
///     tls_ca_file: None,
///     default_ttl: Duration::from_secs(30),
/// }
/// .connect()
/// .await?;
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct DistLockClient {
    pub client_id: String,
    pub password: String,
    pub host: String,
    pub port: u16,
    /// Prefix applied to every lock name (`namespace:...`). `None` or empty: no prefix. Must not contain `:`.
    pub namespace: Option<String>,
    /// TCP connections in the pool (1..=256). They all belong to one session.
    pub pool_size: usize,
    /// Encrypt connections with TLS. The server must have a certificate configured.
    pub tls: bool,
    /// Check the server's certificate against `host` (a DNS name or an IP address that the
    /// certificate lists). `false` still encrypts but accepts any certificate, so it does not stop a
    /// man in the middle. Only used when `tls` is set.
    pub tls_verify: bool,
    /// PEM file with the CA certificate(s) to trust, for a private CA or a self-signed server
    /// certificate. `None`: the public web roots. Only used when `tls` and `tls_verify` are set.
    pub tls_ca_file: Option<std::path::PathBuf>,
    /// Lease length for acquires that do not set their own `ttl`.
    pub default_ttl: Duration,
}

impl Default for DistLockClient {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            password: String::new(),
            host: "127.0.0.1".into(),
            port: 7878,
            namespace: None,
            pool_size: 4,
            tls: false,
            tls_verify: true,
            tls_ca_file: None,
            default_ttl: Duration::from_secs(30),
        }
    }
}

impl std::fmt::Debug for DistLockClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DistLockClient")
            .field("client_id", &self.client_id)
            .field("password", &"<redacted>")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("namespace", &self.namespace)
            .field("pool_size", &self.pool_size)
            .field("tls", &self.tls)
            .field("tls_verify", &self.tls_verify)
            .field("tls_ca_file", &self.tls_ca_file)
            .field("default_ttl", &self.default_ttl)
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct Settings {
    client_id: String,
    password: String,
    host: String,
    port: u16,
    pub(crate) namespace: Option<String>,
    pool_size: usize,
    pub(crate) default_ttl: Duration,
    tls: Option<Tls>,
}

impl DistLockClient {
    pub(crate) fn into_settings(self) -> Result<Settings> {
        let bad = |m: &str| Err(Error::Config(m.to_owned()));
        if self.client_id.is_empty() || self.client_id.len() > 128 {
            return bad("client_id must be 1-128 bytes");
        }
        if self.password.len() > 1024 {
            return bad("password must be at most 1024 bytes");
        }
        if self.host.is_empty() {
            return bad("host must not be empty");
        }
        if self.port == 0 {
            return bad("port must not be 0");
        }
        if !(1..=256).contains(&self.pool_size) {
            return bad("pool_size must be between 1 and 256");
        }
        if self.default_ttl.is_zero() || self.default_ttl > Duration::from_millis(u32::MAX.into()) {
            return bad(
                "default_ttl must be greater than zero (and fit in 32 bits of milliseconds)",
            );
        }
        let namespace = self.namespace.filter(|n| !n.is_empty());
        if namespace.as_deref().is_some_and(|n| n.contains(':')) {
            return bad("namespace must not contain ':'");
        }
        let tls = if self.tls {
            Some(tls::build(
                &self.host,
                self.tls_verify,
                self.tls_ca_file.as_deref(),
            )?)
        } else {
            None
        };
        Ok(Settings {
            client_id: self.client_id,
            password: self.password,
            host: self.host,
            port: self.port,
            namespace,
            pool_size: self.pool_size,
            default_ttl: self.default_ttl,
            tls,
        })
    }

    /// Log in, create the session and start the pool. Fails fast on bad credentials or an
    /// unreachable server; afterwards the pool reconnects on its own. Must run inside a tokio runtime.
    pub async fn connect(self) -> Result<Client> {
        let settings = self.into_settings()?;
        let inner = Inner::new(settings, Handle::current());
        // Created first so that dropping this future (cancellation) also stops the tasks.
        let guard = Arc::new(ShutdownGuard(inner.shutdown.clone()));
        let (first_tx, first_rx) = oneshot::channel();
        inner.spawn_supervisor(0, Some(first_tx));
        match first_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(Error::Closed),
        }
        // The session now exists; the rest of the pool attaches to it.
        for slot in 1..inner.slots.len() {
            inner.spawn_supervisor(slot, None);
        }
        Ok(Client { inner, guard })
    }
}

/// Stops every background task of a client when the last `Client` and `Lock` are gone.
pub(crate) struct ShutdownGuard(CancellationToken);

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

// ------------------------------------------------------------------------------------ the handle

/// A connected, pooled client. Cheap to clone; clones share the pool and the session.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
    guard: Arc<ShutdownGuard>,
}

impl Client {
    /// The server-assigned session id, once logged in. It changes if the session had to be
    /// recreated (after a server restart).
    pub fn session_id(&self) -> Option<u64> {
        self.inner.st().session.as_ref().map(|s| s.id)
    }

    /// Administrative commands, such as reloading the server's users file.
    pub fn maintenance(&self) -> Maintenance {
        Maintenance::new(self.clone())
    }

    /// One maintenance command, one attempt, on one connection. Not retried: a command may end the
    /// sender's own connection, and sending it again on another one would repeat its effect.
    pub(crate) async fn run_maintenance(
        &self,
        command: pb::maintenance::Command,
    ) -> Result<pb::maintenance_ok::Outcome> {
        let conn = self.inner.pick_conn().await?;
        let reply = conn
            .call(
                cf::Body::Maintenance(pb::Maintenance {
                    command: Some(command),
                }),
                MAINTENANCE_TIMEOUT,
            )
            .await?;
        match reply {
            sf::Body::MaintenanceOk(ok) => ok.outcome.ok_or_else(|| protocol("Maintenance")),
            sf::Body::Error(e) => Err(Error::from_server(e)),
            _ => Err(protocol("Maintenance")),
        }
    }

    /// How many of the pool's connections are up right now (at most `pool_size`).
    pub fn connections(&self) -> usize {
        (0..self.inner.slots.len())
            .filter(|&i| self.inner.slot(i).as_ref().is_some_and(|c| !c.is_closed()))
            .count()
    }

    /// How many times a pool connection has been re-established after dropping, since connecting.
    pub fn reconnects(&self) -> u64 {
        self.inner.reconnects.load(Ordering::Relaxed)
    }

    /// Round-trip time to the server over one pool connection. A cheap health check.
    pub async fn ping(&self) -> Result<Duration> {
        let conn = self.inner.pick_conn().await?;
        let started = Instant::now();
        match conn
            .call(cf::Body::Ping(pb::Ping { nonce: 1 }), REQUEST_TIMEOUT)
            .await?
        {
            sf::Body::Pong(_) => Ok(started.elapsed()),
            other => Err(Error::Protocol(format!(
                "unexpected reply to Ping: {other:?}"
            ))),
        }
    }

    /// Wait for the lock. With `wait: None` this waits as long as the server allows; `Some(d)` at
    /// most `d` (then [`Error::Timeout`]); `Some(0)` is a try-lock ([`Error::Busy`]).
    pub async fn acquire(&self, a: Acquire) -> Result<Lock> {
        let req = self.inner.resolve(&a)?;
        let started = Instant::now();
        loop {
            match self.inner.acquire_once(&req, &self.guard).await {
                Err(Error::Unavailable(message)) => {
                    // The server is in its startup grace period: keep trying while the caller's
                    // patience lasts.
                    let budget = req.wait.unwrap_or(UNAVAILABLE_BUDGET);
                    if started.elapsed() + Duration::from_millis(250) >= budget {
                        return Err(Error::Unavailable(message));
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                other => return other,
            }
        }
    }

    /// Like [`acquire`](Self::acquire) with zero wait: `Ok(None)` if the lock is taken.
    pub async fn try_acquire(&self, mut a: Acquire) -> Result<Option<Lock>> {
        a.wait = Some(Duration::ZERO);
        match self.acquire(a).await {
            Ok(lock) => Ok(Some(lock)),
            Err(Error::Busy) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Wait for an event to be set; see [the module docs](crate::event). Returns what the set
    /// carried. With `wait: None` this waits as long as the server allows; `Some(d)` at most `d`
    /// (then [`Error::Timeout`]); `Some(0)` only checks whether the event is published right now.
    ///
    /// Dropping the future withdraws the wait from the server.
    pub async fn wait_event(&self, w: EventWait) -> Result<Published> {
        let wire = self
            .inner
            .event_wire_name(w.namespace.as_deref(), &w.name)?;
        self.inner.event_wait(&wire, w.wait, w.after).await
    }

    /// Set an event: wake every client waiting for it, and publish it for its window.
    ///
    /// A set is not retried when a connection fails mid-request, because the first attempt may have
    /// been applied and repeating it would wake the next waiters a second time. It fails with
    /// [`Error::ConnectionLost`] instead, and whether the event was set is then unknown.
    pub async fn set_event(&self, s: EventSet) -> Result<SetResult> {
        let wire = self
            .inner
            .event_wire_name(s.namespace.as_deref(), &s.name)?;
        self.inner.event_set(wire, s.value, s.publish_for).await
    }

    /// Release every lock this session holds, end the session on the server, and stop the pool.
    /// (Dropping all clones and locks also stops the pool, but leaves the session to expire.)
    pub async fn close(&self) -> Result<()> {
        if let Ok(Ok(conn)) =
            tokio::time::timeout(Duration::from_secs(1), self.inner.pick_conn()).await
        {
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                conn.call(
                    cf::Body::CloseSession(pb::CloseSession {}),
                    Duration::from_secs(3),
                ),
            )
            .await;
        }
        self.inner.invalidate_all();
        self.inner.shutdown.cancel();
        Ok(())
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("session_id", &self.session_id())
            .finish()
    }
}

// ------------------------------------------------------------------------------------ shared state

#[derive(Clone)]
struct SessionInfo {
    id: u64,
    token: Vec<u8>,
    /// Increments each time a *new* session is created; ties local state to one session.
    epoch: u64,
    max_wait: Duration,
}

pub(crate) enum WaitOutcome {
    Granted(pb::Grant),
    /// An event this wait was registered on was set.
    Published(pb::Publication),
    TimedOut,
    SessionLost,
    /// The server no longer has this event wait (it was lost with a connection, or its wake-up was
    /// never delivered): the caller registers it again.
    Missing,
}

struct Waiter {
    epoch: u64,
    /// An event wait, not a lock request: judged against `pending_events` when reconciling.
    event: bool,
    tx: oneshot::Sender<WaitOutcome>,
    /// When the server confirmed the request is queued. Reconciliation only judges waiters that
    /// were queued before the `Attach` it is reconciling (otherwise the view may predate them).
    queued_at: Option<Instant>,
}

struct HeldEntry {
    epoch: u64,
    registered_at: Instant,
    lost: watch::Sender<bool>,
}

#[derive(Default)]
struct Shared {
    session: Option<SessionInfo>,
    epoch: u64,
    waiters: HashMap<u64, Waiter>,
    held: HashMap<u64, HeldEntry>,
}

pub(crate) struct Request {
    pub(crate) user_name: String,
    pub(crate) wire_name: String,
    pub(crate) kind: Kind,
    pub(crate) ttl: Duration,
    pub(crate) wait: Option<Duration>,
}

pub(crate) struct Inner {
    settings: Settings,
    state: Mutex<Shared>,
    slots: Vec<Mutex<Option<Arc<Conn>>>>,
    rr: AtomicUsize,
    request_ids: AtomicU64,
    /// Signalled when a connection comes up or the session changes.
    changed: Notify,
    /// Serialises logins so a cold pool creates exactly one session.
    login_lock: tokio::sync::Mutex<()>,
    /// Connections re-established after the first connect of their slot.
    reconnects: AtomicU64,
    /// Why the most recent connection attempt failed; cleared by the next success.
    last_failure: Mutex<Option<Failure>>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) runtime: Handle,
}

#[derive(Clone)]
enum Failure {
    Auth(String),
    Other(String),
}

/// Why a connection attempt failed.
enum Fail {
    /// Try again immediately (the session changed under us).
    Retry,
    Transient(String),
    Auth(String),
    Fatal(Error),
}

struct Established {
    reader: Reader,
    sink: Writer,
    ping: Duration,
    /// The session view, when we joined an existing session.
    view: Option<pb::SessionView>,
    epoch: u64,
    attach_sent: Instant,
}

impl Inner {
    fn new(settings: Settings, runtime: Handle) -> Arc<Self> {
        let slots = (0..settings.pool_size).map(|_| Mutex::new(None)).collect();
        Arc::new(Self {
            settings,
            state: Mutex::new(Shared::default()),
            slots,
            rr: AtomicUsize::new(0),
            request_ids: AtomicU64::new(1),
            changed: Notify::new(),
            login_lock: tokio::sync::Mutex::new(()),
            reconnects: AtomicU64::new(0),
            last_failure: Mutex::new(None),
            shutdown: CancellationToken::new(),
            runtime,
        })
    }

    fn st(&self) -> MutexGuard<'_, Shared> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn failure(&self) -> MutexGuard<'_, Option<Failure>> {
        self.last_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// What to tell a caller that gave up waiting for a connection: the real reason, if we know it.
    fn not_connected(&self) -> Error {
        match self.failure().clone() {
            Some(Failure::Auth(message)) => Error::Auth(message),
            Some(Failure::Other(reason)) => Error::Connect {
                addr: self.addr(),
                reason,
            },
            None => Error::NotConnected,
        }
    }

    fn slot(&self, i: usize) -> MutexGuard<'_, Option<Arc<Conn>>> {
        self.slots[i].lock().unwrap_or_else(PoisonError::into_inner)
    }

    // -------------------------------------------------------------------------- connections

    fn spawn_supervisor(self: &Arc<Self>, slot: usize, first: Option<oneshot::Sender<Result<()>>>) {
        let inner = Arc::clone(self);
        self.runtime
            .spawn(async move { inner.supervise(slot, first).await });
    }

    /// Keep one pool slot connected for the life of the client.
    async fn supervise(
        self: Arc<Self>,
        slot: usize,
        mut first: Option<oneshot::Sender<Result<()>>>,
    ) {
        let mut backoff = Backoff::default();
        let mut connected_before = false;
        loop {
            let attempt = tokio::select! {
                () = self.shutdown.cancelled() => return,
                r = self.establish() => r,
            };
            let failure = match attempt {
                Ok(est) => {
                    *self.failure() = None;
                    let established_at = Instant::now();
                    if connected_before {
                        self.reconnects.fetch_add(1, Ordering::Relaxed);
                    }
                    connected_before = true;
                    let conn = Conn::spawn(&self, est.reader, est.sink, est.ping);
                    *self.slot(slot) = Some(Arc::clone(&conn));
                    self.changed.notify_waiters();
                    if let Some(tx) = first.take() {
                        let _ = tx.send(Ok(()));
                    }
                    if let Some(view) = est.view {
                        self.reconcile(est.epoch, est.attach_sent, &view);
                    }
                    tokio::select! {
                        () = conn.closed() => {}
                        () = self.shutdown.cancelled() => {
                            conn.close();
                            return;
                        }
                    }
                    *self.slot(slot) = None;
                    if established_at.elapsed() >= STABLE_AFTER {
                        backoff.reset(); // it was healthy for a while: come back at once
                        debug!(slot, "connection lost; reconnecting");
                    } else {
                        // It barely lasted: do not hammer a server that keeps dropping us.
                        let pause = backoff.next();
                        debug!(
                            slot,
                            ?pause,
                            "connection dropped soon after it was made; backing off"
                        );
                        tokio::select! {
                            () = self.shutdown.cancelled() => return,
                            () = tokio::time::sleep(pause) => {}
                        }
                    }
                    continue;
                }
                Err(Fail::Retry) => continue,
                Err(fail) => fail,
            };
            // A failure. The very first login reports it to `connect()`; later ones just retry.
            let (err, text) = match failure {
                Fail::Auth(m) => (Error::Auth(m.clone()), m),
                Fail::Fatal(e) => {
                    let text = e.to_string();
                    (e, text)
                }
                Fail::Transient(reason) => (
                    Error::Connect {
                        addr: self.addr(),
                        reason: reason.clone(),
                    },
                    reason,
                ),
                Fail::Retry => unreachable!("handled above"),
            };
            if let Some(tx) = first.take() {
                let _ = tx.send(Err(err));
                return;
            }
            *self.failure() = Some(match &err {
                Error::Auth(m) => Failure::Auth(m.clone()),
                _ => Failure::Other(text.clone()),
            });
            warn!(slot, "cannot (re)connect to {}: {text}", self.addr());
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                () = tokio::time::sleep(backoff.next()) => {}
            }
        }
    }

    fn addr(&self) -> String {
        format!("{}:{}", self.settings.host, self.settings.port)
    }

    /// Connect and handshake: `Hello` when there is no session yet, otherwise `Attach`.
    async fn establish(self: &Arc<Self>) -> std::result::Result<Established, Fail> {
        let addr = self.addr();
        let stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(Fail::Transient(e.to_string())),
            Err(_) => return Err(Fail::Transient("connection timed out".into())),
        };
        tune_socket(&stream);
        let stream: Box<dyn Io> = match &self.settings.tls {
            None => Box::new(stream),
            Some(t) => {
                let accepted = t.connector.connect(t.server_name.clone(), stream);
                match tokio::time::timeout(CONNECT_TIMEOUT, accepted).await {
                    Ok(Ok(s)) => Box::new(s),
                    Ok(Err(e)) => {
                        return Err(Fail::Transient(format!("TLS handshake failed: {e}")));
                    }
                    Err(_) => return Err(Fail::Transient("TLS handshake timed out".into())),
                }
            }
        };
        let codec = LengthDelimitedCodec::builder()
            .big_endian()
            .length_field_length(4)
            .max_frame_length(MAX_FRAME)
            .new_codec();
        let (read_half, write_half) = tokio::io::split(stream);
        let mut reader: Reader = FramedRead::new(read_half, codec.clone());
        let mut sink: Writer = FramedWrite::new(write_half, codec);

        let session = self.st().session.clone();
        if let Some(session) = session {
            let attach_sent = Instant::now();
            let reply = exchange(
                &mut reader,
                &mut sink,
                cf::Body::Attach(pb::Attach {
                    session_id: session.id,
                    session_token: session.token.clone(),
                }),
            )
            .await?;
            return match reply {
                sf::Body::AttachOk(ok) => Ok(Established {
                    reader,
                    sink,
                    ping: Duration::from_millis(u64::from(ok.ping_interval_ms)),
                    view: Some(ok.view.unwrap_or_default()),
                    epoch: session.epoch,
                    attach_sent,
                }),
                sf::Body::Error(e) => {
                    let code = pb::ErrorCode::try_from(e.code).unwrap_or(pb::ErrorCode::Internal);
                    if matches!(
                        code,
                        pb::ErrorCode::SessionNotFound | pb::ErrorCode::Unauthenticated
                    ) {
                        info!("the server no longer knows our session; logging in again");
                        self.invalidate_session(session.epoch);
                        Err(Fail::Retry)
                    } else {
                        Err(Fail::Transient(format!("attach refused: {}", e.message)))
                    }
                }
                other => Err(Fail::Fatal(Error::Protocol(format!(
                    "unexpected reply to Attach: {other:?}"
                )))),
            };
        }

        let _login = self.login_lock.lock().await;
        if self.st().session.is_some() {
            return Err(Fail::Retry); // another slot logged in while we waited: join its session
        }
        let reply = exchange(
            &mut reader,
            &mut sink,
            cf::Body::Hello(pb::Hello {
                protocol_version: PROTOCOL_VERSION,
                client_id: self.settings.client_id.clone(),
                password: self.settings.password.clone(),
                instance_label: format!("distlock-client/{}", env!("CARGO_PKG_VERSION")),
            }),
        )
        .await?;
        match reply {
            sf::Body::HelloOk(ok) => {
                let epoch = {
                    let mut st = self.st();
                    st.epoch += 1;
                    let epoch = st.epoch;
                    st.session = Some(SessionInfo {
                        id: ok.session_id,
                        token: ok.session_token,
                        epoch,
                        max_wait: Duration::from_millis(u64::from(ok.max_wait_ms)),
                    });
                    epoch
                };
                info!(session = ok.session_id, "logged in");
                self.changed.notify_waiters();
                Ok(Established {
                    reader,
                    sink,
                    ping: Duration::from_millis(u64::from(ok.ping_interval_ms)),
                    view: None,
                    epoch,
                    attach_sent: Instant::now(),
                })
            }
            sf::Body::Error(e) => match Error::from_server(e) {
                Error::Auth(m) => Err(Fail::Auth(m)),
                other => Err(Fail::Fatal(other)),
            },
            other => Err(Fail::Fatal(Error::Protocol(format!(
                "unexpected reply to Hello: {other:?}"
            )))),
        }
    }

    /// Wait until `check` yields a value, a connection or session appears, or we give up.
    async fn wait_until<T>(&self, mut check: impl FnMut() -> Option<T>) -> Result<T> {
        let deadline = Instant::now() + WAIT_FOR_CONNECTION;
        loop {
            if self.shutdown.is_cancelled() {
                return Err(Error::Closed);
            }
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable(); // register before checking, so no wakeup is missed
            if let Some(value) = check() {
                return Ok(value);
            }
            tokio::select! {
                () = &mut notified => {}
                () = tokio::time::sleep_until(deadline) => return Err(self.not_connected()),
                () = self.shutdown.cancelled() => return Err(Error::Closed),
            }
        }
    }

    fn try_pick(&self) -> Option<Arc<Conn>> {
        let n = self.slots.len();
        let start = self.rr.fetch_add(1, Ordering::Relaxed);
        (0..n).find_map(|i| {
            let conn = self.slot((start + i) % n).clone()?;
            (!conn.is_closed()).then_some(conn)
        })
    }

    pub(crate) async fn pick_conn(&self) -> Result<Arc<Conn>> {
        self.wait_until(|| self.try_pick()).await
    }

    async fn session(&self) -> Result<SessionInfo> {
        self.wait_until(|| self.st().session.clone()).await
    }

    /// Send on any connection, retrying on others if the connection dies mid-request. Returns
    /// whether a retry happened (a retried non-idempotent request may have succeeded the first time).
    async fn call_retrying(&self, body: cf::Body) -> Result<(sf::Body, bool)> {
        self.call_retrying_within(body, REQUEST_TIMEOUT).await
    }

    /// Like [`call_retrying`](Self::call_retrying), with each attempt giving up after `timeout`.
    async fn call_retrying_within(
        &self,
        body: cf::Body,
        timeout: Duration,
    ) -> Result<(sf::Body, bool)> {
        let mut retried = false;
        for _ in 0..MAX_RETRIES {
            let conn = self.pick_conn().await?;
            match conn.call(body.clone(), timeout).await {
                Ok(reply) => return Ok((reply, retried)),
                Err(Error::ConnectionLost) => retried = true,
                Err(e) => return Err(e),
            }
        }
        Err(Error::ConnectionLost)
    }

    // -------------------------------------------------------------------------- session state

    /// The session is gone: every waiter and every held lock tied to it is lost.
    fn invalidate_session(&self, epoch: u64) {
        let mut st = self.st();
        if st.session.as_ref().map(|s| s.epoch) != Some(epoch) {
            return;
        }
        st.session = None;
        for (_, w) in st.waiters.drain().filter(|(_, w)| w.epoch == epoch) {
            let _ = w.tx.send(WaitOutcome::SessionLost);
        }
        for (_, h) in st.held.drain().filter(|(_, h)| h.epoch == epoch) {
            let _ = h.lost.send(true);
        }
    }

    pub(crate) fn invalidate_all(&self) {
        let epoch = self.st().session.as_ref().map(|s| s.epoch);
        if let Some(epoch) = epoch {
            self.invalidate_session(epoch);
        }
    }

    pub(crate) fn mark_lost(&self, token: u64) {
        if let Some(h) = self.st().held.remove(&token) {
            let _ = h.lost.send(true);
        }
    }

    pub(crate) fn forget_held(&self, token: u64) {
        self.st().held.remove(&token);
    }

    /// Compare what we believe with what the server says the session holds and waits for.
    /// This is the "reverify after reconnect" step: nothing that happened while we were
    /// disconnected goes unnoticed.
    fn reconcile(&self, epoch: u64, attach_sent: Instant, view: &pb::SessionView) {
        let mut st = self.st();
        if st.session.as_ref().map(|s| s.epoch) != Some(epoch) {
            return;
        }
        let held_by_request: HashMap<u64, &pb::HeldLock> =
            view.held.iter().map(|h| (h.request_id, h)).collect();
        let held_tokens: std::collections::HashSet<u64> =
            view.held.iter().map(|h| h.fencing_token).collect();
        let pending: std::collections::HashSet<u64> =
            view.pending.iter().map(|p| p.request_id).collect();
        let pending_events: std::collections::HashSet<u64> =
            view.pending_events.iter().map(|p| p.request_id).collect();

        // Waiters whose fate the server decided while we were away (or whose push was lost).
        let decided: Vec<(u64, WaitOutcome)> = st
            .waiters
            .iter()
            .filter(|(_, w)| w.epoch == epoch && w.queued_at.is_some_and(|q| q < attach_sent))
            .filter_map(|(rid, w)| {
                if w.event {
                    // Registered on the server, or not: if not, it was woken or timed out while we
                    // were away and we were not told. The waiter asks again, and a publication
                    // that is still inside its window answers at once.
                    return (!pending_events.contains(rid)).then_some((*rid, WaitOutcome::Missing));
                }
                match held_by_request.get(rid) {
                    Some(h) => Some((
                        *rid,
                        WaitOutcome::Granted(pb::Grant {
                            request_id: h.request_id,
                            name: h.name.clone(),
                            fencing_token: h.fencing_token,
                            ttl_remaining_ms: h.ttl_remaining_ms,
                            previous: h.previous,
                        }),
                    )),
                    None if pending.contains(rid) => None, // still queued: keep waiting
                    None => Some((*rid, WaitOutcome::TimedOut)),
                }
            })
            .collect();
        for (rid, outcome) in decided {
            if let Some(w) = st.waiters.remove(&rid) {
                let _ = w.tx.send(outcome);
            }
        }

        // Locks we hold that the server no longer lists: their lease ran out while we were away.
        let lost: Vec<u64> = st
            .held
            .iter()
            .filter(|(t, h)| {
                h.epoch == epoch && h.registered_at < attach_sent && !held_tokens.contains(t)
            })
            .map(|(t, _)| *t)
            .collect();
        for token in lost {
            if let Some(h) = st.held.remove(&token) {
                let _ = h.lost.send(true);
            }
        }
    }

    /// Server pushes, from whichever connection delivered them.
    pub(crate) fn on_push(self: &Arc<Self>, body: sf::Body) {
        match body {
            sf::Body::Granted(g) => {
                if let Some(grant) = g.grant {
                    let waiter = self.st().waiters.remove(&grant.request_id);
                    let delivered = match waiter {
                        Some(w) => w.tx.send(WaitOutcome::Granted(grant.clone())).is_ok(),
                        None => false,
                    };
                    if !delivered {
                        // Nobody wants it any more (the acquire was cancelled): hand it back.
                        self.release_in_background(grant.name, grant.fencing_token);
                    }
                }
            }
            sf::Body::EventPublished(p) => {
                // A wait that was cancelled meanwhile has nobody to tell, and nothing to hand back.
                let waiter = self.st().waiters.remove(&p.request_id);
                if let (Some(w), Some(publication)) = (waiter, p.publication) {
                    let _ = w.tx.send(WaitOutcome::Published(publication));
                }
            }
            sf::Body::WaitTimedOut(t) => {
                let waiter = self.st().waiters.remove(&t.request_id);
                if let Some(w) = waiter {
                    let _ = w.tx.send(WaitOutcome::TimedOut);
                }
            }
            sf::Body::LeaseExpired(l) => self.mark_lost(l.fencing_token),
            sf::Body::Shutdown(s) => {
                info!(drain_ms = s.drain_ms, "the server announced a shutdown")
            }
            other => debug!("unexpected push: {other:?}"),
        }
    }

    // -------------------------------------------------------------------------- operations

    pub(crate) fn resolve(&self, a: &Acquire) -> Result<Request> {
        if a.name.is_empty() {
            return Err(Error::InvalidArgument("lock name must not be empty".into()));
        }
        if let Kind::Semaphore { capacity, permits } = a.kind
            && (capacity == 0 || permits == 0 || permits > capacity)
        {
            return Err(Error::InvalidArgument(
                "a semaphore needs capacity > 0 and 0 < permits <= capacity".into(),
            ));
        }
        let ttl = a.ttl.unwrap_or(self.settings.default_ttl);
        if ttl.is_zero() {
            return Err(Error::InvalidArgument(
                "ttl must be greater than zero".into(),
            ));
        }
        let namespace = match &a.namespace {
            Some(n) => Some(n.as_str()).filter(|n| !n.is_empty()),
            None => self.settings.namespace.as_deref(),
        };
        if namespace.is_some_and(|n| n.contains(':')) {
            return Err(Error::InvalidArgument(
                "namespace must not contain ':'".into(),
            ));
        }
        Ok(Request {
            user_name: a.name.clone(),
            wire_name: wire_name(namespace, a.kind, &a.name),
            kind: a.kind,
            ttl,
            wait: a.wait,
        })
    }

    pub(crate) async fn acquire_once(
        self: &Arc<Self>,
        req: &Request,
        guard: &Arc<ShutdownGuard>,
    ) -> Result<Lock> {
        let session = self.session().await?;
        let request_id = self.request_ids.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.st();
            if st.session.as_ref().map(|s| s.epoch) != Some(session.epoch) {
                return Err(Error::SessionLost);
            }
            // Registered *before* the request is sent, so a push can never beat us to it.
            st.waiters.insert(
                request_id,
                Waiter {
                    epoch: session.epoch,
                    event: false,
                    tx,
                    queued_at: None,
                },
            );
        }
        // If this future is dropped from here on, take the request back so it cannot be granted to nobody.
        let mut pending = PendingGuard {
            inner: Arc::clone(self),
            request_id,
            cancel: true,
        };

        let (mode, capacity, permits) = req.kind.to_pb();
        let sent_at = Instant::now();
        let (reply, _) = self
            .call_retrying(cf::Body::Acquire(pb::Acquire {
                request_id,
                name: req.wire_name.clone(),
                mode: mode as i32,
                ttl_ms: millis(req.ttl),
                wait_ms: req.wait.map_or(u32::MAX, millis), // u32::MAX: as long as the server allows
                semaphore_capacity: capacity,
                permits,
            }))
            .await?;

        // The lease clock: an immediate grant happened after we sent the request, so counting from
        // the send is conservative; a queued grant happened at some unknown earlier moment, so the
        // best we can do is count from when we learn of it.
        let (grant, base) = match reply {
            sf::Body::AcquireOk(pb::AcquireOk {
                outcome: Some(pb::acquire_ok::Outcome::Granted(g)),
            }) => {
                pending.settle();
                (g, sent_at)
            }
            sf::Body::AcquireOk(pb::AcquireOk {
                outcome: Some(pb::acquire_ok::Outcome::Queued(_)),
            }) => {
                if let Some(w) = self.st().waiters.get_mut(&request_id) {
                    w.queued_at = Some(Instant::now());
                }
                let wait = req
                    .wait
                    .map_or(session.max_wait, |w| w.min(session.max_wait));
                match tokio::time::timeout(wait + WAIT_SLACK, rx).await {
                    Ok(Ok(WaitOutcome::Granted(g))) => {
                        pending.settle();
                        (g, Instant::now())
                    }
                    Ok(Ok(WaitOutcome::TimedOut)) => {
                        pending.settle();
                        return Err(Error::Timeout);
                    }
                    Ok(Ok(WaitOutcome::SessionLost) | Err(_)) => {
                        pending.settle();
                        return Err(Error::SessionLost);
                    }
                    Ok(Ok(WaitOutcome::Published(_) | WaitOutcome::Missing)) => {
                        pending.settle();
                        return Err(Error::Protocol(
                            "an event outcome arrived for a lock request".into(),
                        ));
                    }
                    // The server never said anything: give up (the guard cancels the request).
                    Err(_) => return Err(Error::Timeout),
                }
            }
            sf::Body::Error(e) => {
                pending.settle();
                return Err(Error::from_server(e));
            }
            other => {
                return Err(Error::Protocol(format!(
                    "unexpected reply to Acquire: {other:?}"
                )));
            }
        };
        Ok(self.make_lock(&grant, req, session.epoch, base, guard))
    }

    /// `namespace:event:name`, after the same checks as for locks.
    fn event_wire_name(&self, namespace: Option<&str>, name: &str) -> Result<String> {
        if name.is_empty() {
            return Err(Error::InvalidArgument(
                "event name must not be empty".into(),
            ));
        }
        let namespace = match namespace {
            Some(n) => Some(n).filter(|n| !n.is_empty()),
            None => self.settings.namespace.as_deref(),
        };
        if namespace.is_some_and(|n| n.contains(':')) {
            return Err(Error::InvalidArgument(
                "namespace must not contain ':'".into(),
            ));
        }
        Ok(family_name(namespace, "event", name))
    }

    /// Wait for an event, registering again if the server loses the wait while we are reconnecting.
    pub(crate) async fn event_wait(
        self: &Arc<Self>,
        wire_name: &str,
        wait: Option<Duration>,
        after: u64,
    ) -> Result<Published> {
        let session = self.session().await?;
        let budget = wait.map_or(session.max_wait, |w| w.min(session.max_wait));
        let deadline = Instant::now() + budget;
        let mut first = true;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() && !first {
                return Err(Error::Timeout);
            }
            first = false;
            if let Some(published) = self.event_wait_attempt(wire_name, after, remaining).await? {
                return Ok(published);
            }
        }
    }

    /// One registration. `Ok(None)`: the server lost the wait; register it again.
    async fn event_wait_attempt(
        self: &Arc<Self>,
        wire_name: &str,
        after: u64,
        wait: Duration,
    ) -> Result<Option<Published>> {
        let session = self.session().await?;
        let request_id = self.request_ids.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.st();
            if st.session.as_ref().map(|s| s.epoch) != Some(session.epoch) {
                return Err(Error::SessionLost);
            }
            // Registered *before* the request is sent, so a push can never beat us to it.
            st.waiters.insert(
                request_id,
                Waiter {
                    epoch: session.epoch,
                    event: true,
                    tx,
                    queued_at: None,
                },
            );
        }
        // If this future is dropped from here on, withdraw the wait from the server.
        let mut pending = PendingGuard {
            inner: Arc::clone(self),
            request_id,
            cancel: true,
        };

        let (reply, _) = self
            .call_retrying(cf::Body::EventWait(pb::EventWait {
                request_id,
                name: wire_name.to_owned(),
                wait_ms: millis(wait),
                after_id: after,
            }))
            .await?;
        match reply {
            sf::Body::EventWaitOk(pb::EventWaitOk {
                outcome: Some(pb::event_wait_ok::Outcome::Published(p)),
            }) => {
                pending.settle();
                Ok(Some(p.into()))
            }
            sf::Body::EventWaitOk(pb::EventWaitOk {
                outcome: Some(pb::event_wait_ok::Outcome::Queued(_)),
            }) => {
                if let Some(w) = self.st().waiters.get_mut(&request_id) {
                    w.queued_at = Some(Instant::now());
                }
                match tokio::time::timeout(wait + WAIT_SLACK, rx).await {
                    Ok(Ok(WaitOutcome::Published(p))) => {
                        pending.settle();
                        Ok(Some(p.into()))
                    }
                    Ok(Ok(WaitOutcome::TimedOut)) => {
                        pending.settle();
                        Err(Error::Timeout)
                    }
                    Ok(Ok(WaitOutcome::Missing)) => {
                        pending.settle();
                        Ok(None)
                    }
                    Ok(Ok(WaitOutcome::SessionLost) | Err(_)) => {
                        pending.settle();
                        Err(Error::SessionLost)
                    }
                    Ok(Ok(WaitOutcome::Granted(_))) => {
                        pending.settle();
                        Err(Error::Protocol(
                            "a lock grant arrived for an event wait".into(),
                        ))
                    }
                    // The server never said anything: give up (the guard withdraws the wait).
                    Err(_) => Err(Error::Timeout),
                }
            }
            // A zero wait found nothing published: that is a timeout, not a busy lock.
            sf::Body::Error(e) if e.code == pb::ErrorCode::Busy as i32 => {
                pending.settle();
                Err(Error::Timeout)
            }
            sf::Body::Error(e) => {
                pending.settle();
                Err(Error::from_server(e))
            }
            other => Err(Error::Protocol(format!(
                "unexpected reply to EventWait: {other:?}"
            ))),
        }
    }

    /// One attempt, on one connection, never retried (see [`Client::set_event`]).
    pub(crate) async fn event_set(
        &self,
        wire_name: String,
        value: Option<String>,
        publish_for: Option<Duration>,
    ) -> Result<SetResult> {
        let conn = self.pick_conn().await?;
        let reply = conn
            .call(
                cf::Body::EventSet(pb::EventSet {
                    name: wire_name,
                    value,
                    publish_ms: publish_for.map(millis),
                }),
                REQUEST_TIMEOUT,
            )
            .await?;
        match reply {
            sf::Body::EventSetOk(ok) => Ok(SetResult {
                woken: ok.woken,
                id: ok.publication_id,
                publish_for: Duration::from_millis(u64::from(ok.publish_ms)),
            }),
            sf::Body::Error(e) => Err(Error::from_server(e)),
            other => Err(Error::Protocol(format!(
                "unexpected reply to EventSet: {other:?}"
            ))),
        }
    }

    fn make_lock(
        self: &Arc<Self>,
        grant: &pb::Grant,
        req: &Request,
        epoch: u64,
        base: Instant,
        guard: &Arc<ShutdownGuard>,
    ) -> Lock {
        let token = grant.fencing_token;
        let granted_ttl =
            Duration::from_millis(u64::from(grant.ttl_remaining_ms)).max(Duration::from_millis(1));
        let (lost_tx, lost_rx) = watch::channel(false);
        {
            let mut st = self.st();
            if st.session.as_ref().map(|s| s.epoch) == Some(epoch) {
                st.held.insert(
                    token,
                    HeldEntry {
                        epoch,
                        registered_at: Instant::now(),
                        lost: lost_tx.clone(),
                    },
                );
            } else {
                let _ = lost_tx.send(true); // the session was replaced while the grant was in flight
            }
        }
        let stop = CancellationToken::new();
        self.runtime.spawn(renew_loop(
            Arc::clone(self),
            req.wire_name.clone(),
            token,
            req.ttl,
            granted_ttl,
            base + granted_ttl,
            lost_tx,
            stop.clone(),
        ));
        Lock {
            inner: Arc::clone(self),
            _guard: Arc::clone(guard),
            name: req.user_name.clone(),
            wire_name: req.wire_name.clone(),
            kind: req.kind,
            token,
            previous: grant.previous.map(|p| Previous {
                state: State::from_pb(p.state).unwrap_or(State::Abandoned),
                set_by_token: p.set_by_token,
                age: Duration::from_millis(u64::from(p.age_ms)),
            }),
            ttl: granted_ttl,
            lost: lost_rx,
            stop,
            done: false,
        }
    }

    pub(crate) async fn extend_once(
        &self,
        wire_name: &str,
        token: u64,
        ttl: Duration,
        within: Duration,
    ) -> Result<Duration> {
        let (reply, _) = self
            .call_retrying_within(
                cf::Body::Extend(pb::Extend {
                    items: vec![pb::ExtendItem {
                        name: wire_name.to_owned(),
                        fencing_token: token,
                        ttl_ms: millis(ttl),
                    }],
                }),
                within,
            )
            .await?;
        match reply {
            sf::Body::ExtendOk(mut ok) if ok.results.len() == 1 => {
                let r = ok.results.remove(0);
                match pb::ErrorCode::try_from(r.code) {
                    Ok(pb::ErrorCode::Ok) => {
                        Ok(Duration::from_millis(u64::from(r.ttl_remaining_ms)))
                    }
                    Ok(pb::ErrorCode::NotHeld) => Err(Error::LeaseLost),
                    _ => Err(Error::Server {
                        code: pb::ErrorCode::try_from(r.code).unwrap_or(pb::ErrorCode::Internal),
                        message: "extend failed".into(),
                        existing_capacity: 0,
                    }),
                }
            }
            sf::Body::Error(e) => Err(Error::from_server(e)),
            other => Err(Error::Protocol(format!(
                "unexpected reply to Extend: {other:?}"
            ))),
        }
    }

    pub(crate) async fn release_raw(
        &self,
        wire_name: &str,
        token: u64,
        state: pb::HolderState,
        state_ttl: Option<Duration>,
    ) -> Result<()> {
        let (reply, retried) = self
            .call_retrying(cf::Body::Release(pb::Release {
                name: wire_name.to_owned(),
                fencing_token: token,
                state: state as i32,
                state_ttl_ms: state_ttl.map_or(0, millis),
            }))
            .await?;
        match reply {
            sf::Body::ReleaseOk(_) => Ok(()),
            // We resent the release after a connection died: the first copy probably got through.
            sf::Body::Error(e) if retried && e.code == pb::ErrorCode::NotHeld as i32 => Ok(()),
            sf::Body::Error(e) => Err(Error::from_server(e)),
            other => Err(Error::Protocol(format!(
                "unexpected reply to Release: {other:?}"
            ))),
        }
    }

    pub(crate) fn release_in_background(self: &Arc<Self>, wire_name: String, token: u64) {
        let inner = Arc::clone(self);
        self.runtime.spawn(async move {
            let _ = inner
                .release_raw(&wire_name, token, pb::HolderState::Unset, None)
                .await;
        });
    }
}

/// While a request may still be in flight or queued, undoes it if the acquiring future is dropped.
struct PendingGuard {
    inner: Arc<Inner>,
    request_id: u64,
    cancel: bool,
}

impl PendingGuard {
    /// The request reached a definitive outcome; nothing to undo.
    fn settle(&mut self) {
        self.cancel = false;
        self.inner.st().waiters.remove(&self.request_id);
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.inner.st().waiters.remove(&self.request_id);
        if self.cancel {
            // Dequeues a waiting request, or releases the lock if it was granted in the meantime.
            let inner = Arc::clone(&self.inner);
            let request_id = self.request_id;
            self.inner.runtime.spawn(async move {
                let _ = inner
                    .call_retrying(cf::Body::CancelAcquire(pb::CancelAcquire { request_id }))
                    .await;
            });
        }
    }
}

/// Keeps one lease alive: extend at a third of its length until released or lost.
#[allow(clippy::too_many_arguments)]
async fn renew_loop(
    inner: Arc<Inner>,
    wire_name: String,
    token: u64,
    requested_ttl: Duration,
    granted_ttl: Duration,
    first_expiry: Instant,
    lost: watch::Sender<bool>,
    stop: CancellationToken,
) {
    let interval = (granted_ttl / 3).max(Duration::from_millis(5));
    let retry = interval.min(Duration::from_millis(100));
    let mut expiry = first_expiry;
    let mut next = interval;
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(next) => {}
        }
        if *lost.borrow() {
            return;
        }
        let sent = Instant::now();
        // One attempt may not take longer than the renewal interval, however long a hung connection
        // or a missing connection would otherwise make it. A lease is a few intervals long, so the
        // retries that follow must have time to land.
        let budget = interval.max(Duration::from_millis(250));
        let result = match tokio::time::timeout(
            budget,
            inner.extend_once(&wire_name, token, requested_ttl, budget),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Error::ConnectionLost),
        };
        match result {
            Ok(remaining) => {
                expiry = sent + remaining;
                next = interval;
            }
            Err(Error::LeaseLost | Error::SessionLost | Error::Closed) => {
                inner.mark_lost(token);
                return;
            }
            Err(e) => {
                debug!(token, "could not extend the lease: {e}");
                if Instant::now() >= expiry {
                    // We cannot prove the lease is alive any more: treat it as lost.
                    inner.mark_lost(token);
                    return;
                }
                next = retry;
            }
        }
    }
}

fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

/// One request/response on a fresh connection, before the reader and writer tasks exist.
async fn exchange(
    reader: &mut Reader,
    sink: &mut Writer,
    body: cf::Body,
) -> std::result::Result<sf::Body, Fail> {
    let frame = pb::ClientFrame {
        seq: 1,
        body: Some(body),
    };
    match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        sink.send(Bytes::from(frame.encode_to_vec())),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(Fail::Transient(e.to_string())),
        Err(_) => return Err(Fail::Transient("handshake timed out".into())),
    }
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, reader.next()).await {
        Err(_) => Err(Fail::Transient("handshake timed out".into())),
        Ok(None) => Err(Fail::Transient(
            "the server closed the connection during the handshake".into(),
        )),
        Ok(Some(Err(e))) => Err(Fail::Transient(e.to_string())),
        Ok(Some(Ok(bytes))) => match pb::ServerFrame::decode(&bytes[..]) {
            Ok(f) => f.body.ok_or_else(|| {
                Fail::Fatal(Error::Protocol("empty reply during the handshake".into()))
            }),
            Err(e) => Err(Fail::Fatal(Error::Protocol(format!(
                "undecodable reply: {e}"
            )))),
        },
    }
}

/// Exponential reconnect backoff (100 ms .. 5 s) with ±25 % jitter, so a restarted server is not
/// hit by every client at once.
struct Backoff {
    current: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            current: Duration::from_millis(100),
        }
    }
}

impl Backoff {
    fn next(&mut self) -> Duration {
        let d = self.current;
        self.current = (self.current * 2).min(Duration::from_secs(5));
        let mut byte = [128u8];
        let _ = getrandom::fill(&mut byte);
        d.mul_f64(0.75 + f64::from(byte[0]) / 255.0 * 0.5)
    }

    fn reset(&mut self) {
        self.current = Duration::from_millis(100);
    }
}
