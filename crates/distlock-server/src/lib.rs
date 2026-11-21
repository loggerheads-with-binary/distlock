//! `distlock-server`: the TCP front end of the lock service.
//!
//! * [`ServerConfig`]: settings from environment variables (`.env.example`).
//! * [`UserDb`]: the users file (`config/users.json`) and bcrypt checks.
//! * [`Server`]: accepts connections, speaks the protobuf protocol, and drives one engine task
//!   that owns a [`distlock_core::LockTable`].
//!
//! See `DESIGN.md` for the semantics.

mod actor;
mod config;
mod conn;
mod convert;
mod server;
mod stats;
mod tls;
mod users;
mod util;

pub use config::{ConfigError, LogFormat, ServerConfig};
pub use server::Server;
pub use users::{UserDb, UsersError};
