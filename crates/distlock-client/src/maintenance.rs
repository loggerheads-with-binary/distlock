//! Administrative commands, reached through [`Client::maintenance`](crate::Client::maintenance).
//!
//! There is no per-user authorization yet, so any logged-in client may send them. Each command is
//! therefore safe and idempotent: the worst a caller can do is ask the server to re-read a file an
//! administrator controls.

use crate::client::Client;
use crate::error::{Error, Result};
use distlock_proto::v1 as pb;

/// What a [`Maintenance::reset_users`] did. Counts only: the names are in the server's log, so this
/// call cannot be used to enumerate other applications.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResetUsersReport {
    pub added: u32,
    pub removed: u32,
    pub disabled: u32,
    pub enabled: u32,
    pub password_changed: u32,
    /// Sessions of removed or disabled users that were ended (their locks released, with the
    /// server's lock-delay, and their waiting requests cancelled).
    pub sessions_closed: u32,
    /// Their TCP connections, closed by the server.
    pub connections_closed: u32,
    /// The sender itself was among the users thrown out. This reply was sent first; its connection
    /// closes right after, and so does every other connection of its pool.
    pub invoker_dropped: bool,
    /// MD5 of the users file now in force on the server (32 lowercase hex characters; empty if the
    /// server does not report it). Compare it with the hash of the file you meant to deploy.
    pub users_md5: String,
}

impl ResetUsersReport {
    /// Nothing changed at all: no user was added, removed or changed. The hash is not a change.
    pub fn is_noop(&self) -> bool {
        Self {
            users_md5: String::new(),
            ..self.clone()
        } == Self::default()
    }
}

impl From<pb::ResetUsersOk> for ResetUsersReport {
    fn from(r: pb::ResetUsersOk) -> Self {
        Self {
            added: r.added,
            removed: r.removed,
            disabled: r.disabled,
            enabled: r.enabled,
            password_changed: r.password_changed,
            sessions_closed: r.sessions_closed,
            connections_closed: r.connections_closed,
            invoker_dropped: r.invoker_dropped,
            users_md5: r.users_md5,
        }
    }
}

/// Administrative commands. Cheap to create and to clone.
#[derive(Clone)]
pub struct Maintenance {
    client: Client,
}

impl Maintenance {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Tell the server to re-read its users file and swap it in.
    ///
    /// Logins are never refused or delayed meanwhile. Users that are gone from the file, or now
    /// disabled, are thrown out: their sessions end and **every one of their TCP connections is
    /// closed by the server, including this client's own if it was thrown out** (the report still
    /// arrives, with `invoker_dropped` set; the client then fails to log in again and calls report
    /// [`Error::Auth`]). Live sessions of users whose password merely changed are left alone.
    ///
    /// If the file cannot be loaded nothing changes and this fails with [`Error::Maintenance`].
    /// Safe to repeat: an unchanged file is a no-op. It is deliberately *not* retried on a lost
    /// connection, because the command may end the sender's own connection; call it again if you
    /// need to.
    pub async fn reset_users(&self) -> Result<ResetUsersReport> {
        let command = pb::maintenance::Command::ResetUsers(pb::ResetUsers {});
        match self.client.run_maintenance(command).await? {
            pb::maintenance_ok::Outcome::ResetUsers(report) => Ok(report.into()),
        }
    }
}

pub(crate) fn protocol(what: &str) -> Error {
    Error::Protocol(format!("unexpected reply to {what}"))
}
