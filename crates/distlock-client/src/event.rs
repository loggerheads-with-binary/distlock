//! Events: named broadcasts, like `asyncio.Event` with a limited memory.
//!
//! Any number of clients [wait](crate::Client::wait_event) on a name. One client
//! [sets](crate::Client::set_event) it, which wakes everyone waiting at that moment and returns how
//! many there were. For the *publish window* after the set, the event also counts as published: a
//! wait that starts inside the window returns at once with the same value. After the window the
//! event is gone, and waiters wait for the next set.
//!
//! ```no_run
//! # async fn demo(client: distlock_client::Client) -> distlock_client::Result<()> {
//! use std::time::Duration;
//! use distlock_client::{EventSet, EventWait};
//!
//! // One client, or many:
//! let published = client.wait_event(EventWait::new("deploy-finished").wait(Duration::from_secs(60))).await?;
//! println!("deployed {:?}", published.value);
//!
//! // Another one, later:
//! let set = client
//!     .set_event(EventSet::new("deploy-finished").value("v1.2.3").publish_for(Duration::from_secs(30)))
//!     .await?;
//! println!("woke {} waiting clients", set.woken);
//! # Ok(()) }
//! ```
//!
//! Event names live in their own family on the wire (`namespace:event:name`), so an event called
//! `x` never interacts with a lock called `x`.
//!
//! # Delivery
//!
//! A wait belongs to the session, not to a connection: it survives the connection that carried it,
//! and the wake-up is delivered on whichever connection is attached. If the session has no
//! connection at the moment of the set (a reconnect is under way), the wake-up cannot be delivered;
//! the client then waits again, and is answered at once if the publish window is still open. So an
//! event that must not be missed should be set with a window longer than a reconnect takes
//! (the default is 10 s), and a consumer that loops should pass the id of the last publication it
//! handled as [`EventWait::after`], so the same publication is not delivered twice.

use std::time::Duration;

use distlock_proto::v1 as pb;

use crate::lock::Wait;

/// A request to wait for an event.
#[derive(Debug, Clone)]
pub struct EventWait {
    pub name: String,
    /// How long to wait; see [`Wait`]. Left alone, the [defaults](crate::defaults) apply, and with
    /// none set that is as long as the server allows. `Wait::For(Duration::ZERO)`: do not wait;
    /// succeeds only if the event is inside its publish window right now.
    pub wait: Wait,
    /// Only a publication with an id greater than this ends the wait at once. Pass
    /// [`Published::id`] of the last publication handled, to skip it while it is still published.
    pub after: u64,
    /// Overrides the client's namespace. `Some("")` means no namespace at all.
    pub namespace: Option<String>,
}

impl EventWait {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            wait: Wait::Default,
            after: 0,
            namespace: None,
        }
    }

    pub fn wait(mut self, wait: Duration) -> Self {
        self.wait = Wait::For(wait);
        self
    }

    /// Wait as long as the server allows, even if a shorter default wait has been set.
    pub fn wait_forever(mut self) -> Self {
        self.wait = Wait::Forever;
        self
    }

    pub fn after(mut self, id: u64) -> Self {
        self.after = id;
        self
    }

    pub fn namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }
}

/// A request to set (publish) an event.
#[derive(Debug, Clone)]
pub struct EventSet {
    pub name: String,
    /// Delivered to every waiter. Capped in size by the server.
    pub value: Option<String>,
    /// How long after this set the event stays published for late waiters. `None`: the
    /// [default](crate::defaults::set_default_event_publish_for) if one is set, else the server's. `Some(0)`: wake only the clients waiting now. The server caps long windows.
    pub publish_for: Option<Duration>,
    /// Overrides the client's namespace. `Some("")` means no namespace at all.
    pub namespace: Option<String>,
}

impl EventSet {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: None,
            publish_for: None,
            namespace: None,
        }
    }

    pub fn value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    pub fn publish_for(mut self, window: Duration) -> Self {
        self.publish_for = Some(window);
        self
    }

    pub fn namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }
}

/// An event, as a waiter sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// Strictly increasing across every set and every lock grant on the server. Pass it as
    /// [`EventWait::after`] to wait for something newer.
    pub id: u64,
    /// The value given to the set, if any.
    pub value: Option<String>,
    /// How long ago it was set. Zero for a waiter woken by the set itself.
    pub age: Duration,
}

impl From<pb::Publication> for Published {
    fn from(p: pb::Publication) -> Self {
        Self {
            id: p.id,
            value: p.value,
            age: Duration::from_millis(u64::from(p.age_ms)),
        }
    }
}

/// What a set did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetResult {
    /// Clients that were waiting, and have been woken by this set.
    pub woken: u32,
    /// The publication's id; the same id every woken waiter sees.
    pub id: u64,
    /// The publish window the server applied (after its default and its cap).
    pub publish_for: Duration,
}
