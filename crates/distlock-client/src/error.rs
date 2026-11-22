use distlock_proto::v1 as pb;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    Config(String),
    /// TLS could not be set up (bad CA file, bad server name) or the server's certificate was
    /// rejected under the verification settings in use.
    #[error("TLS: {0}")]
    Tls(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("cannot connect to {addr}: {reason}")]
    Connect { addr: String, reason: String },
    /// No connection became available in time (the client keeps reconnecting in the background).
    #[error("not connected to the server")]
    NotConnected,
    #[error("the connection to the server was lost")]
    ConnectionLost,
    /// The server no longer knows our session (it restarted, or the session expired), so every
    /// lock held through it is gone.
    #[error(
        "the session no longer exists (server restarted or session expired): locks held through it are gone"
    )]
    SessionLost,
    /// Waited as long as asked (or as long as the server allows) without getting the lock.
    #[error("timed out waiting for the lock")]
    Timeout,
    /// A try-lock (zero wait) found the lock taken.
    #[error("the lock is held by someone else")]
    Busy,
    /// The lease expired, or the lock was already released.
    #[error("the lock is not held: its lease expired or it was already released")]
    LeaseLost,
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// The server is not accepting lock requests yet (startup grace period after a restart).
    #[error("the server is not ready: {0}")]
    Unavailable(String),
    #[error("server error {code:?}: {message}")]
    Server {
        code: pb::ErrorCode,
        message: String,
        existing_capacity: u32,
    },
    /// A maintenance command could not be carried out (for example the server's users file could not
    /// be loaded). Nothing was changed.
    #[error("maintenance command failed: {0}")]
    Maintenance(String),
    /// The server logged us in, but it is older than the `min_version` this client asks for.
    #[error("the server is too old: it reports {server}, this client needs at least {required}")]
    ServerTooOld { server: String, required: String },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("the client has been closed")]
    Closed,
}

impl Error {
    /// A short, stable identifier for each kind of error, for language bindings and logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Config(_) => "config",
            Error::Tls(_) => "tls",
            Error::Auth(_) => "auth",
            Error::Connect { .. } => "connect",
            Error::NotConnected => "not_connected",
            Error::ConnectionLost => "connection_lost",
            Error::SessionLost => "session_lost",
            Error::Timeout => "timeout",
            Error::Busy => "busy",
            Error::LeaseLost => "lease_lost",
            Error::InvalidArgument(_) => "invalid_argument",
            Error::Unavailable(_) => "unavailable",
            Error::Server { .. } => "server",
            Error::Maintenance(_) => "maintenance",
            Error::ServerTooOld { .. } => "server_too_old",
            Error::Protocol(_) => "protocol",
            Error::Closed => "closed",
        }
    }

    pub(crate) fn from_server(e: pb::Error) -> Error {
        let code = pb::ErrorCode::try_from(e.code).unwrap_or(pb::ErrorCode::Internal);
        match code {
            pb::ErrorCode::Busy => Error::Busy,
            pb::ErrorCode::NotHeld => Error::LeaseLost,
            pb::ErrorCode::Unauthenticated => Error::Auth(e.message),
            pb::ErrorCode::SessionNotFound => Error::SessionLost,
            pb::ErrorCode::InvalidArgument => Error::InvalidArgument(e.message),
            pb::ErrorCode::Unavailable => Error::Unavailable(e.message),
            pb::ErrorCode::MaintenanceFailed => Error::Maintenance(e.message),
            _ => Error::Server {
                code,
                message: e.message,
                existing_capacity: e.existing_capacity,
            },
        }
    }
}
