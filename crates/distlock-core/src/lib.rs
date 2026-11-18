//! `distlock-core`: the lock-manager state machine.
//!
//! Pure and synchronous: no IO, no clock, no async runtime. See `DESIGN.md` for the semantics.
//!
//! ```
//! use std::time::Duration;
//! use distlock_core::*;
//!
//! let mut table = LockTable::new(Config::default());
//! let now = Duration::ZERO;
//! let status = table.acquire(now, AcquireRequest {
//!     session: SessionId(1),
//!     request: RequestId(1),
//!     name: "jobs/nightly".into(),
//!     want: Want::Mutex,
//!     ttl: Duration::from_secs(30),
//!     wait: Duration::from_secs(5),
//! }).unwrap();
//! let AcquireStatus::Granted(grant) = status else { unreachable!() };
//! table.release(now, SessionId(1), "jobs/nightly", grant.token, Some(ReleaseState::Succeeded), None).unwrap();
//! ```

mod table;
mod types;

pub use table::{Config, LockTable};
pub use types::*;
