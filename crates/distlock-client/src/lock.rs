//! What you ask for ([`Acquire`]) and what you get ([`Lock`]).

use std::sync::Arc;
use std::time::Duration;

use distlock_proto::v1 as pb;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::client::{Inner, ShutdownGuard};
use crate::conn::REQUEST_TIMEOUT;
use crate::error::{Error, Result};

/// The kind of lock. A name is only ever used with one *family* at a time: mutexes, read/write
/// locks and semaphores live in separate namespaces, so a mutex called `x` and a read/write lock
/// called `x` do not interact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Exclusive. Separate from read/write locks of the same name.
    Mutex,
    /// Shared. Blocks, and is blocked by, `Write` locks of the same name.
    Read,
    /// Exclusive. Blocks, and is blocked by, `Read` and `Write` locks of the same name.
    Write,
    /// `permits` of `capacity`. The first acquirer defines the capacity; the semaphore disappears
    /// when its last holder and waiter are gone.
    Semaphore { capacity: u32, permits: u32 },
}

impl Kind {
    /// Which family a name belongs to on the wire.
    fn tag(self) -> &'static str {
        match self {
            Kind::Mutex => "mutex",
            Kind::Read | Kind::Write => "rw",
            Kind::Semaphore { .. } => "sem",
        }
    }

    /// Only exclusive holders can leave a status behind.
    pub fn is_exclusive(self) -> bool {
        matches!(self, Kind::Mutex | Kind::Write)
    }

    pub(crate) fn to_pb(self) -> (pb::Mode, u32, u32) {
        match self {
            Kind::Mutex => (pb::Mode::Mutex, 0, 0),
            Kind::Read => (pb::Mode::Read, 0, 0),
            Kind::Write => (pb::Mode::Write, 0, 0),
            Kind::Semaphore { capacity, permits } => (pb::Mode::Semaphore, capacity, permits),
        }
    }
}

/// The name the server sees: `namespace:family:name`. The family keeps mutexes, read/write locks
/// and semaphores apart; the namespace keeps applications apart.
pub(crate) fn wire_name(namespace: Option<&str>, kind: Kind, name: &str) -> String {
    match namespace {
        Some(ns) => format!("{ns}:{}:{name}", kind.tag()),
        None => format!("{}:{name}", kind.tag()),
    }
}

/// The outcome a holder leaves for whoever takes the lock next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Succeeded,
    Failed,
    Stored,
    /// Assigned by the server when a holder's lease expired or its session closed while holding.
    /// Readable, but never settable.
    Abandoned,
}

impl State {
    pub(crate) fn from_pb(v: i32) -> Option<State> {
        match pb::HolderState::try_from(v).ok()? {
            pb::HolderState::Unset => None,
            pb::HolderState::Succeeded => Some(State::Succeeded),
            pb::HolderState::Failed => Some(State::Failed),
            pb::HolderState::Stored => Some(State::Stored),
            pb::HolderState::Abandoned => Some(State::Abandoned),
        }
    }

    pub(crate) fn to_pb(self) -> pb::HolderState {
        match self {
            State::Succeeded => pb::HolderState::Succeeded,
            State::Failed => pb::HolderState::Failed,
            State::Stored => pb::HolderState::Stored,
            State::Abandoned => pb::HolderState::Abandoned,
        }
    }
}

/// What the previous holder left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Previous {
    pub state: State,
    /// Fencing token of the grant that set it.
    pub set_by_token: u64,
    /// How long ago it was set.
    pub age: Duration,
}

/// A request for a lock.
#[derive(Debug, Clone)]
pub struct Acquire {
    pub name: String,
    pub kind: Kind,
    /// Lease length (kept alive automatically while the [`Lock`] lives). `None`: the client's `default_ttl`.
    pub ttl: Option<Duration>,
    /// How long to wait. `None`: as long as the server allows. `Some(0)`: do not wait.
    pub wait: Option<Duration>,
    /// Overrides the client's namespace. `Some("")` means no namespace at all.
    pub namespace: Option<String>,
}

impl Acquire {
    pub fn new(name: impl Into<String>, kind: Kind) -> Self {
        Self {
            name: name.into(),
            kind,
            ttl: None,
            wait: None,
            namespace: None,
        }
    }

    pub fn mutex(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Mutex)
    }

    pub fn read(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Read)
    }

    pub fn write(name: impl Into<String>) -> Self {
        Self::new(name, Kind::Write)
    }

    pub fn semaphore(name: impl Into<String>, capacity: u32, permits: u32) -> Self {
        Self::new(name, Kind::Semaphore { capacity, permits })
    }

    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    pub fn wait(mut self, wait: Duration) -> Self {
        self.wait = Some(wait);
        self
    }

    pub fn namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }
}

/// A cloneable view of a [`Lock`]'s health, from [`Lock::watch_lost`].
#[derive(Clone, Debug)]
pub struct Lost(watch::Receiver<bool>);

impl Lost {
    pub fn is_lost(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once the lease is lost. (It also resolves once the lock has been released or
    /// dropped, since there is then nothing left to watch.)
    pub async fn wait(&self) {
        let mut rx = self.0.clone();
        let _ = rx.wait_for(|lost| *lost).await;
    }
}

/// A held lock. The lease is renewed in the background until the lock is released or dropped.
///
/// Dropping a `Lock` releases it (best effort, without a status); prefer [`release`](Lock::release)
/// or [`release_with`](Lock::release_with), which report errors.
pub struct Lock {
    pub(crate) inner: Arc<Inner>,
    pub(crate) _guard: Arc<ShutdownGuard>,
    pub(crate) name: String,
    pub(crate) wire_name: String,
    pub(crate) kind: Kind,
    pub(crate) token: u64,
    pub(crate) previous: Option<Previous>,
    pub(crate) ttl: Duration,
    pub(crate) lost: watch::Receiver<bool>,
    pub(crate) stop: CancellationToken,
    pub(crate) done: bool,
}

impl Lock {
    /// The name you asked for (without namespace or family prefix).
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Strictly increasing across every grant of every lock on the server. Pass it to the resource
    /// you protect and have that resource reject writes carrying an older token.
    pub fn fencing_token(&self) -> u64 {
        self.token
    }

    /// The previous holder's status, if it left one that has not expired.
    pub fn previous(&self) -> Option<Previous> {
        self.previous
    }

    /// The lease length the server actually granted (it may cap what you asked for).
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Whether the lease is known to be gone (expired, or the session was lost). Once true, stop
    /// using whatever the lock protects.
    pub fn is_lost(&self) -> bool {
        *self.lost.borrow()
    }

    /// Resolves when the lease is lost. Never resolves for a lock that stays healthy and held.
    pub async fn wait_lost(&self) {
        self.watch_lost().wait().await;
    }

    /// A cloneable handle on this lock's health that does not borrow the lock, so it can be awaited
    /// while the lock itself is used or released elsewhere.
    pub fn watch_lost(&self) -> Lost {
        Lost(self.lost.clone())
    }

    /// Push the lease out to `ttl` from now. (The background renewal already does this; call it
    /// only to lengthen the lease beyond what you asked for at acquire time.)
    pub async fn extend(&self, ttl: Duration) -> Result<Duration> {
        let res = self
            .inner
            .extend_once(&self.wire_name, self.token, ttl, REQUEST_TIMEOUT)
            .await;
        if matches!(res, Err(Error::LeaseLost)) {
            self.inner.mark_lost(self.token);
        }
        res
    }

    /// Release, leaving any existing status untouched.
    pub async fn release(self) -> Result<()> {
        self.release_with(None, None).await
    }

    /// Release and leave `state` for the next holder (mutex and write locks only), readable for
    /// `state_ttl` (`None`: the server default). Fails with [`Error::LeaseLost`] if the lease had
    /// already expired.
    pub async fn release_with(
        mut self,
        state: Option<State>,
        state_ttl: Option<Duration>,
    ) -> Result<()> {
        let rejected = state.and_then(|s| {
            if s == State::Abandoned {
                Some("Abandoned is assigned by the server and cannot be set")
            } else if !self.kind.is_exclusive() {
                Some("only mutex and write locks can leave a status")
            } else {
                None
            }
        });
        if let Some(reason) = rejected {
            // The lock is consumed either way, so give it back (without a status) before reporting
            // the mistake; otherwise the release would trail behind the error.
            self.finish();
            let _ = self
                .inner
                .release_raw(&self.wire_name, self.token, pb::HolderState::Unset, None)
                .await;
            return Err(Error::InvalidArgument(reason.into()));
        }
        self.finish();
        // The RPC runs as its own task. If the caller's future is dropped while it is in flight (an
        // async task being cancelled), the release still completes; otherwise the lock would sit
        // there until its lease expired, and then trigger the server's lock-delay.
        let inner = Arc::clone(&self.inner);
        let wire_name = self.wire_name.clone();
        let token = self.token;
        let state = state.map_or(pb::HolderState::Unset, State::to_pb);
        let task = self
            .inner
            .runtime
            .spawn(async move { inner.release_raw(&wire_name, token, state, state_ttl).await });
        task.await.unwrap_or(Err(Error::Closed))
    }

    fn finish(&mut self) {
        self.done = true;
        self.stop.cancel();
        self.inner.forget_held(self.token);
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if !self.done {
            self.finish();
            self.inner
                .release_in_background(self.wire_name.clone(), self.token);
        }
    }
}

impl std::fmt::Debug for Lock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lock")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("fencing_token", &self.token)
            .field("lost", &self.is_lost())
            .finish()
    }
}
