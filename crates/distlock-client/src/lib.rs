//! `distlock-client`: a small, functional client for the distlock server.
//!
//! ```no_run
//! # async fn demo() -> distlock_client::Result<()> {
//! use distlock_client::{Acquire, DistLockClient, State};
//!
//! let client = DistLockClient {
//!     client_id: "billing-service".into(),
//!     password: "secret".into(),
//!     ..Default::default()
//! }
//! .connect()
//! .await?;
//!
//! let lock = client.acquire(Acquire::mutex("nightly-report")).await?;
//! if lock.previous().map(|p| p.state) == Some(State::Succeeded) {
//!     // somebody already did the work
//! }
//! // ... work; the lease is renewed for you, and `lock.is_lost()` tells you if it was lost ...
//! lock.release_with(Some(State::Succeeded), None).await?;
//! # Ok(()) }
//! ```
//!
//! * Async API: [`DistLockClient::connect`], [`Client`], [`Lock`].
//! * Blocking API: [`DistLockClient::connect_blocking`], [`blocking::Client`], [`blocking::Lock`].
//!
//! One client is one *session* on the server, carried by `pool_size` TCP connections. Locks belong
//! to the session, so any connection can serve any request; dropped connections are re-established
//! and the session is re-joined, and the locks you hold are re-verified against the server.
//!
//! Lock names are sent to the server as `namespace:family:name`, where the family is `mutex`,
//! `rw` or `sem`. That is why a mutex and a read/write lock with the same name never conflict.

pub mod blocking;
mod client;
mod conn;
mod error;
pub mod event;
mod lock;
mod maintenance;
mod tls;
mod url;
mod version;

pub use client::{Client, DistLockClient};
/// What [`Client::stats`] returns.
pub use distlock_proto::v1::StatsOk as ServerStats;
pub use error::{Error, Result};
pub use event::{EventSet, EventWait, Published, SetResult};
pub use lock::{Acquire, Kind, Lock, Lost, Previous, State};
pub use maintenance::{Maintenance, ResetUsersReport};
