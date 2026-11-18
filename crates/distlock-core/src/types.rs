//! Public vocabulary of the lock table.

use std::{sync::Arc, time::Duration};

/// Monotonic time since an arbitrary epoch (the server uses `Instant` since process start).
/// The table never reads a clock: callers pass the current time into every operation.
pub type Time = Duration;

/// Lock name. Cheap to clone; shared between the name table, timers and events.
pub type Name = Arc<str>;

macro_rules! id_type {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u64);
    };
}

id_type!(
    /// Owner of locks: one per client process (its connection pool). Server-generated, never reused.
    SessionId
);
id_type!(
    /// Client-chosen, unique per session. Makes `acquire` retries idempotent.
    RequestId
);
id_type!(
    /// Issued at every grant from one server-wide, strictly increasing counter.
    /// Doubles as the handle for `release` / `extend`.
    FencingToken
);

/// What a client asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    Mutex,
    /// Shared mode of an RwLock.
    Read,
    /// Exclusive mode of an RwLock.
    Write,
    Semaphore {
        /// Defined by the first acquirer; later acquirers must match while the semaphore exists.
        capacity: u32,
        permits: u32,
    },
}

impl Want {
    pub(crate) fn kind(self) -> Kind {
        match self {
            Want::Mutex => Kind::Mutex,
            Want::Read | Want::Write => Kind::RwLock,
            Want::Semaphore { capacity, .. } => Kind::Semaphore { capacity },
        }
    }

    pub(crate) fn is_exclusive(self) -> bool {
        matches!(self, Want::Mutex | Want::Write)
    }

    pub(crate) fn permits(self) -> u64 {
        match self {
            Want::Semaphore { permits, .. } => u64::from(permits),
            _ => 1,
        }
    }
}

/// What a name *is*. Fixed for as long as the name exists in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mutex,
    RwLock,
    Semaphore { capacity: u32 },
}

/// Tags a holder may leave when releasing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseState {
    Succeeded,
    Failed,
    Stored,
}

/// Tag visible to the next acquirer. `Abandoned` is server-assigned only
/// (lease expired, or session closed while still holding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HolderState {
    Succeeded,
    Failed,
    Stored,
    Abandoned,
}

impl From<ReleaseState> for HolderState {
    fn from(s: ReleaseState) -> Self {
        match s {
            ReleaseState::Succeeded => HolderState::Succeeded,
            ReleaseState::Failed => HolderState::Failed,
            ReleaseState::Stored => HolderState::Stored,
        }
    }
}

/// The previous holder's tag, as seen by a new grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviousState {
    pub state: HolderState,
    /// Fencing token of the grant that set it.
    pub set_by: FencingToken,
    pub age: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantInfo {
    pub token: FencingToken,
    pub ttl_remaining: Duration,
    pub previous: Option<PreviousState>,
}

#[derive(Debug, Clone)]
pub struct AcquireRequest {
    pub session: SessionId,
    pub request: RequestId,
    pub name: Name,
    pub want: Want,
    /// Lease length, starting at the moment of the grant. Clamped to `Config::max_ttl`.
    pub ttl: Duration,
    /// Max time to wait in the queue. Zero is try-lock. Clamped to `Config::max_wait`.
    pub wait: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireStatus {
    Granted(GrantInfo),
    /// Wait for `Event::Granted` or `Event::WaitTimedOut`.
    Queued,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    NotFound,
    /// Was waiting; removed from the queue.
    Dequeued,
    /// Had already been granted; the grant was released (no tag, no lock-delay).
    ReleasedGrant,
}

/// Asynchronous things that happened as a side effect of an operation or the passage of time.
/// Drain them with [`LockTable::take_events`](crate::LockTable::take_events) after every call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A queued request was granted.
    Granted {
        session: SessionId,
        request: RequestId,
        name: Name,
        grant: GrantInfo,
    },
    /// A queued request ran out of patience and was removed.
    WaitTimedOut {
        session: SessionId,
        request: RequestId,
        name: Name,
    },
    /// A lease ran out. The holder must stop using the protected resource.
    LeaseExpired {
        session: SessionId,
        name: Name,
        token: FencingToken,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldGrant {
    pub request: RequestId,
    pub name: Name,
    pub want: Want,
    pub token: FencingToken,
    pub ttl_remaining: Duration,
    /// What the grant reported when it was made, so a client that missed the `Granted` message
    /// (it reconnected) still learns the previous holder's tag.
    pub previous: Option<PreviousState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRequest {
    pub request: RequestId,
    pub name: Name,
    pub want: Want,
}

/// What the table believes a session holds and waits for. Returned on reconnect (reclaim).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionView {
    pub held: Vec<HeldGrant>,
    pub pending: Vec<PendingRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub entries: usize,
    pub sessions: usize,
    pub timers: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid argument: {0}")]
    InvalidArgument(&'static str),
    #[error("lock is busy")]
    Busy,
    #[error("name exists with a different kind: {existing:?}")]
    KindMismatch { existing: Kind },
    #[error("semaphore exists with capacity {existing}")]
    CapacityMismatch { existing: u32 },
    #[error("lock is not held by this session with this token")]
    NotHeld,
    #[error("request id reused with different parameters")]
    RequestConflict,
    #[error("resource exhausted: {0}")]
    ResourceExhausted(&'static str),
    #[error("server is in its startup grace period")]
    GracePeriod,
    #[error("internal inconsistency: {0}")]
    Internal(&'static str),
}
