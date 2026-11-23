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
//!
//! # Primitives and defaults
//!
//! A [`Primitive`] ([`MutexPrimitive`], [`ReadLockPrimitive`], [`WriteLockPrimitive`],
//! [`SemaphorePrimitive`]; also [`Acquire`], [`EventWait`] and [`EventSet`]) describes *what* you
//! want without a client, so one definition can be bound again and again, to the same client or to
//! different ones:
//!
//! ```no_run
//! # async fn demo(a: distlock_client::Client, b: distlock_client::Client) -> distlock_client::Result<()> {
//! use distlock_client::{Attach, Primitive, ReadLockPrimitive};
//!
//! let readers = ReadLockPrimitive::new("catalog");
//! let lock = readers.bind(&a).await?;   // the primitive binds itself to a client...
//! drop(lock);
//! let lock = a.attach(&readers).await?; // ...or the client attaches the primitive
//! drop(lock);
//! let lock = b.attach(&readers).await?; // and the same primitive works on another client
//! # drop(lock); Ok(()) }
//! ```
//!
//! What a primitive leaves out (`ttl`, `wait`, the event publish window) comes from the
//! process-wide [`defaults`], then from the client.

pub mod blocking;
mod client;
mod conn;
pub mod defaults;
mod error;
pub mod event;
mod lock;
mod maintenance;
pub mod primitive;
mod tls;
mod url;
mod version;

pub use client::{Client, DistLockClient};
/// What [`Client::stats`] returns.
pub use distlock_proto::v1::StatsOk as ServerStats;
pub use error::{Error, Result};
pub use event::{EventSet, EventWait, Published, SetResult};
pub use lock::{Acquire, Kind, Lock, Lost, Previous, State, Wait};
pub use maintenance::{Maintenance, ResetUsersReport};
pub use primitive::{
    Attach, AttachBlocking, MutexPrimitive, Primitive, ReadLockPrimitive, SemaphorePrimitive,
    WriteLockPrimitive,
};
