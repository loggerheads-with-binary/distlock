//! The same client for code that is not async. Each [`Client`] owns a small tokio runtime and
//! blocks the calling thread.
//!
//! Do not call these from inside an async task (tokio forbids blocking there, and dropping the
//! client there panics); use the async [`crate::Client`] instead.

use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};

use crate::client::DistLockClient;
use crate::error::{Error, Result};
use crate::lock::{Acquire, Kind, Previous, State};
use crate::maintenance::ResetUsersReport;

impl DistLockClient {
    /// Like [`connect`](DistLockClient::connect), but blocking.
    pub fn connect_blocking(self) -> Result<Client> {
        let rt = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("distlock-client")
            .enable_all()
            .build()
            .map_err(|e| Error::Config(format!("cannot start the client runtime: {e}")))?;
        let inner = rt.block_on(self.connect())?;
        Ok(Client {
            inner,
            rt: Arc::new(rt),
        })
    }
}

#[derive(Clone)]
pub struct Client {
    inner: crate::Client,
    rt: Arc<Runtime>,
}

impl Client {
    pub fn session_id(&self) -> Option<u64> {
        self.inner.session_id()
    }

    pub fn acquire(&self, a: Acquire) -> Result<Lock> {
        let lock = self.rt.block_on(self.inner.acquire(a))?;
        Ok(Lock {
            inner: Some(lock),
            rt: Arc::clone(&self.rt),
        })
    }

    pub fn try_acquire(&self, a: Acquire) -> Result<Option<Lock>> {
        Ok(self
            .rt
            .block_on(self.inner.try_acquire(a))?
            .map(|lock| Lock {
                inner: Some(lock),
                rt: Arc::clone(&self.rt),
            }))
    }

    /// Administrative commands, such as reloading the server's users file.
    pub fn maintenance(&self) -> Maintenance {
        Maintenance {
            inner: self.inner.maintenance(),
            rt: Arc::clone(&self.rt),
        }
    }

    pub fn close(&self) -> Result<()> {
        self.rt.block_on(self.inner.close())
    }
}

pub struct Lock {
    /// `None` only after the lock was consumed by a release.
    inner: Option<crate::Lock>,
    rt: Arc<Runtime>,
}

impl Lock {
    fn lock(&self) -> &crate::Lock {
        self.inner
            .as_ref()
            .expect("lock is only taken by release, which consumes self")
    }

    pub fn name(&self) -> &str {
        self.lock().name()
    }

    pub fn kind(&self) -> Kind {
        self.lock().kind()
    }

    pub fn fencing_token(&self) -> u64 {
        self.lock().fencing_token()
    }

    pub fn previous(&self) -> Option<Previous> {
        self.lock().previous()
    }

    pub fn ttl(&self) -> Duration {
        self.lock().ttl()
    }

    pub fn is_lost(&self) -> bool {
        self.lock().is_lost()
    }

    /// Block until the lease is lost.
    pub fn wait_lost(&self) {
        self.rt.block_on(self.lock().wait_lost());
    }

    pub fn extend(&self, ttl: Duration) -> Result<Duration> {
        self.rt.block_on(self.lock().extend(ttl))
    }

    pub fn release(self) -> Result<()> {
        self.release_with(None, None)
    }

    pub fn release_with(mut self, state: Option<State>, state_ttl: Option<Duration>) -> Result<()> {
        let lock = self.inner.take().expect("release consumes the lock");
        self.rt.block_on(lock.release_with(state, state_ttl))
    }
}

impl std::fmt::Debug for Lock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f)
    }
}

/// Blocking administrative commands; see [`crate::Maintenance`].
#[derive(Clone)]
pub struct Maintenance {
    inner: crate::Maintenance,
    rt: Arc<Runtime>,
}

impl Maintenance {
    /// Tell the server to re-read its users file; see [`crate::Maintenance::reset_users`].
    pub fn reset_users(&self) -> Result<ResetUsersReport> {
        self.rt.block_on(self.inner.reset_users())
    }
}
