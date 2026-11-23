//! Primitives: what you want from the server, written down without a client.
//!
//! A [`Primitive`] is a reusable description ("the read lock called `catalog`, 20 s lease, wait at
//! most 5 s"). It holds no connection, so it can be built once, shared, and bound as often as you
//! like, to one client or to several. Binding performs the operation and returns its result: a
//! [`Lock`] for a lock primitive, [`Published`] for an [`EventWait`], [`SetResult`] for
//! an [`EventSet`].
//!
//! There are two ways to bind, and they do the same thing:
//!
//! ```no_run
//! # async fn demo(client: distlock_client::Client) -> distlock_client::Result<()> {
//! use std::time::Duration;
//! use distlock_client::{Attach, Primitive, ReadLockPrimitive};
//!
//! let readers = ReadLockPrimitive::new("catalog").wait(Duration::from_secs(5));
//!
//! let lock = readers.bind(&client).await?;     // the primitive binds itself to a client
//! lock.release().await?;
//! let lock = client.attach(&readers).await?;   // the client attaches a primitive
//! lock.release().await?;
//! # Ok(()) }
//! ```
//!
//! [`Attach`] (async) and [`AttachBlocking`] are blanket-implemented for everything that is
//! `AsRef<Client>` / `AsRef<blocking::Client>`: a client, an `Arc` of one, and so on.
//!
//! Whatever a primitive leaves unset (`ttl`, `wait`) follows the [defaults](crate::defaults) when it
//! is bound, not when it was built.

use std::future::Future;
use std::time::Duration;

use crate::blocking;
use crate::client::Client;
use crate::error::Result;
use crate::event::{EventSet, EventWait, Published, SetResult};
use crate::lock::{Acquire, Kind, Lock, Wait};

/// A client-less description of one operation. Implemented by the lock primitives in this module
/// and by [`Acquire`], [`EventWait`] and [`EventSet`].
pub trait Primitive {
    /// What binding to an async [`Client`] produces.
    type Output;
    /// What binding to a [`blocking::Client`] produces.
    type BlockingOutput;

    /// Perform the operation on `client`. The primitive is not consumed: bind it again whenever you like.
    fn bind<'a>(
        &'a self,
        client: &'a Client,
    ) -> impl Future<Output = Result<Self::Output>> + Send + 'a;

    /// Like [`bind`](Self::bind), blocking the calling thread.
    fn bind_blocking(&self, client: &blocking::Client) -> Result<Self::BlockingOutput>;
}

/// The client's side of [`Primitive::bind`]: `client.attach(&primitive)`. Implemented for every
/// `AsRef<Client>`.
pub trait Attach {
    fn attach<'a, P: Primitive>(
        &'a self,
        primitive: &'a P,
    ) -> impl Future<Output = Result<P::Output>> + Send + 'a;
}

impl<C: AsRef<Client> + Sync> Attach for C {
    fn attach<'a, P: Primitive>(
        &'a self,
        primitive: &'a P,
    ) -> impl Future<Output = Result<P::Output>> + Send + 'a {
        primitive.bind(self.as_ref())
    }
}

/// The blocking counterpart of [`Attach`], for every `AsRef<blocking::Client>`.
pub trait AttachBlocking {
    fn attach<P: Primitive>(&self, primitive: &P) -> Result<P::BlockingOutput>;
}

impl<C: AsRef<blocking::Client>> AttachBlocking for C {
    fn attach<P: Primitive>(&self, primitive: &P) -> Result<P::BlockingOutput> {
        primitive.bind_blocking(self.as_ref())
    }
}

impl AsRef<Client> for Client {
    fn as_ref(&self) -> &Client {
        self
    }
}

impl AsRef<blocking::Client> for blocking::Client {
    fn as_ref(&self) -> &blocking::Client {
        self
    }
}

// ------------------------------------------------------------------------------------ acquire etc.

impl Primitive for Acquire {
    type Output = Lock;
    type BlockingOutput = blocking::Lock;

    fn bind<'a>(&'a self, client: &'a Client) -> impl Future<Output = Result<Lock>> + Send + 'a {
        client.acquire(self.clone())
    }

    fn bind_blocking(&self, client: &blocking::Client) -> Result<blocking::Lock> {
        client.acquire(self.clone())
    }
}

impl Primitive for EventWait {
    type Output = Published;
    type BlockingOutput = Published;

    fn bind<'a>(
        &'a self,
        client: &'a Client,
    ) -> impl Future<Output = Result<Published>> + Send + 'a {
        client.wait_event(self.clone())
    }

    fn bind_blocking(&self, client: &blocking::Client) -> Result<Published> {
        client.wait_event(self.clone())
    }
}

impl Primitive for EventSet {
    type Output = SetResult;
    type BlockingOutput = SetResult;

    fn bind<'a>(
        &'a self,
        client: &'a Client,
    ) -> impl Future<Output = Result<SetResult>> + Send + 'a {
        client.set_event(self.clone())
    }

    fn bind_blocking(&self, client: &blocking::Client) -> Result<SetResult> {
        client.set_event(self.clone())
    }
}

// ------------------------------------------------------------------------------------ lock primitives

/// The settings every lock primitive shares, and what they turn into.
macro_rules! lock_primitive {
    ($(#[$doc:meta])* $name:ident, $kind:expr) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name {
            pub name: String,
            /// `None`: the [defaults](crate::defaults), then the client's `default_ttl`.
            pub ttl: Option<Duration>,
            /// See [`Wait`].
            pub wait: Wait,
            /// Overrides the client's namespace. `Some("")` means no namespace at all.
            pub namespace: Option<String>,
        }

        impl $name {
            pub fn new(name: impl Into<String>) -> Self {
                Self {
                    name: name.into(),
                    ttl: None,
                    wait: Wait::Default,
                    namespace: None,
                }
            }

            /// Lease length (kept alive automatically while the lock is held).
            pub fn ttl(mut self, ttl: Duration) -> Self {
                self.ttl = Some(ttl);
                self
            }

            /// Wait at most this long (`Duration::ZERO`: do not wait).
            pub fn wait(mut self, wait: Duration) -> Self {
                self.wait = Wait::For(wait);
                self
            }

            /// Wait as long as the server allows, even if a shorter default wait is set.
            pub fn wait_forever(mut self) -> Self {
                self.wait = Wait::Forever;
                self
            }

            pub fn namespace(mut self, namespace: impl Into<String>) -> Self {
                self.namespace = Some(namespace.into());
                self
            }

            /// The request this primitive stands for.
            pub fn to_acquire(&self) -> Acquire {
                Acquire {
                    name: self.name.clone(),
                    kind: $kind(self),
                    ttl: self.ttl,
                    wait: self.wait,
                    namespace: self.namespace.clone(),
                }
            }

            /// Like [`Primitive::bind`] without waiting: `Ok(None)` if the lock is taken.
            pub async fn try_bind(&self, client: &Client) -> Result<Option<Lock>> {
                client.try_acquire(self.to_acquire()).await
            }

            /// Blocking [`try_bind`](Self::try_bind).
            pub fn try_bind_blocking(&self, client: &blocking::Client) -> Result<Option<blocking::Lock>> {
                client.try_acquire(self.to_acquire())
            }
        }

        impl Primitive for $name {
            type Output = Lock;
            type BlockingOutput = blocking::Lock;

            fn bind<'a>(&'a self, client: &'a Client) -> impl Future<Output = Result<Lock>> + Send + 'a {
                client.acquire(self.to_acquire())
            }

            fn bind_blocking(&self, client: &blocking::Client) -> Result<blocking::Lock> {
                client.acquire(self.to_acquire())
            }
        }

        impl From<$name> for Acquire {
            fn from(p: $name) -> Acquire {
                p.to_acquire()
            }
        }
    };
}

lock_primitive!(
    /// An exclusive lock. A mutex is a separate space from read/write locks of the same name.
    MutexPrimitive,
    |_: &MutexPrimitive| Kind::Mutex
);

lock_primitive!(
    /// The shared side of a read/write lock: many readers at once, never alongside a writer.
    ReadLockPrimitive,
    |_: &ReadLockPrimitive| Kind::Read
);

lock_primitive!(
    /// The exclusive side of a read/write lock: blocks, and is blocked by, readers and writers.
    WriteLockPrimitive,
    |_: &WriteLockPrimitive| Kind::Write
);

/// At most `capacity` holders at a time, each holding `permits`. The first acquirer defines the
/// capacity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemaphorePrimitive {
    pub name: String,
    pub capacity: u32,
    pub permits: u32,
    /// `None`: the [defaults](crate::defaults), then the client's `default_ttl`.
    pub ttl: Option<Duration>,
    /// See [`Wait`].
    pub wait: Wait,
    /// Overrides the client's namespace. `Some("")` means no namespace at all.
    pub namespace: Option<String>,
}

impl SemaphorePrimitive {
    /// A semaphore taking one permit per holder.
    pub fn new(name: impl Into<String>, capacity: u32) -> Self {
        Self {
            name: name.into(),
            capacity,
            permits: 1,
            ttl: None,
            wait: Wait::Default,
            namespace: None,
        }
    }

    /// How many permits each holder takes.
    pub fn permits(mut self, permits: u32) -> Self {
        self.permits = permits;
        self
    }

    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    pub fn wait(mut self, wait: Duration) -> Self {
        self.wait = Wait::For(wait);
        self
    }

    pub fn wait_forever(mut self) -> Self {
        self.wait = Wait::Forever;
        self
    }

    pub fn namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }

    /// The request this primitive stands for.
    pub fn to_acquire(&self) -> Acquire {
        Acquire {
            name: self.name.clone(),
            kind: Kind::Semaphore {
                capacity: self.capacity,
                permits: self.permits,
            },
            ttl: self.ttl,
            wait: self.wait,
            namespace: self.namespace.clone(),
        }
    }

    /// Like [`Primitive::bind`] without waiting: `Ok(None)` if no permits are free.
    pub async fn try_bind(&self, client: &Client) -> Result<Option<Lock>> {
        client.try_acquire(self.to_acquire()).await
    }

    /// Blocking [`try_bind`](Self::try_bind).
    pub fn try_bind_blocking(&self, client: &blocking::Client) -> Result<Option<blocking::Lock>> {
        client.try_acquire(self.to_acquire())
    }
}

impl Primitive for SemaphorePrimitive {
    type Output = Lock;
    type BlockingOutput = blocking::Lock;

    fn bind<'a>(&'a self, client: &'a Client) -> impl Future<Output = Result<Lock>> + Send + 'a {
        client.acquire(self.to_acquire())
    }

    fn bind_blocking(&self, client: &blocking::Client) -> Result<blocking::Lock> {
        client.acquire(self.to_acquire())
    }
}

impl From<SemaphorePrimitive> for Acquire {
    fn from(p: SemaphorePrimitive) -> Acquire {
        p.to_acquire()
    }
}
