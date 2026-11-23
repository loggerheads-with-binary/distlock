//! Process-wide default lease lengths, wait times and publish windows.
//!
//! Every request that does not say what it wants (`Acquire::ttl`, `Acquire::wait`,
//! [`EventWait::wait`](crate::EventWait::wait), [`EventSet::publish_for`](crate::EventSet::publish_for))
//! falls back to these, *at the moment the request is made*, so changing a default affects
//! primitives and requests that were built earlier.
//!
//! ```
//! use std::time::Duration;
//! use distlock_client::defaults::{set_default_semaphore_wait, set_default_wait};
//!
//! set_default_wait(Some(Duration::from_secs(10)));           // every lock and event
//! set_default_semaphore_wait(Some(Duration::from_secs(60))); // ...but semaphores wait longer
//! # distlock_client::defaults::reset_all();
//! ```
//!
//! # Cascade
//!
//! For one request the first value that is set wins:
//!
//! | what | order |
//! |---|---|
//! | wait of a lock | the request's own `wait`, the default for its kind (`mutex`, `read`, `write`, `semaphore`), [`set_default_wait`], then *as long as the server allows* |
//! | wait of an event | the request's own `wait`, [`set_default_event_wait`], [`set_default_wait`], then *as long as the server allows* |
//! | lease length | the request's own `ttl`, the default for its kind, [`set_default_ttl`], then the client's [`default_ttl`](crate::DistLockClient::default_ttl) |
//! | publish window | the request's own `publish_for`, [`set_default_event_publish_for`], then the server's default |
//!
//! With nothing set anywhere, behaviour is exactly what it was before these defaults existed.
//!
//! A wait default of `None` means *wait as long as the server allows*. It is different from never
//! having set one: a kind that was set to `None` stops the cascade there, while a kind that is
//! [reset](reset_default_semaphore_wait) lets it continue to the next level.
//!
//! # Storage
//!
//! Each default is one `AtomicU64` (a duration in nanoseconds, with two reserved values for "unset"
//! and "forever"), so reading and writing them never blocks. Nothing here needs a lock.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::lock::Kind;

/// The server counts leases in 32-bit milliseconds.
const MAX_TTL: Duration = Duration::from_millis(u32::MAX as u64);

const UNSET: u64 = u64::MAX;
const FOREVER: u64 = u64::MAX - 1;

/// One default: unset, "forever" (a wait of `None`), or a duration.
struct Slot(AtomicU64);

impl Slot {
    const fn new() -> Self {
        Slot(AtomicU64::new(UNSET))
    }

    fn set(&self, value: Option<Duration>) {
        let raw = match value {
            None => FOREVER,
            // Saturate below the two reserved values (about 584 years).
            Some(d) => u64::try_from(d.as_nanos()).map_or(FOREVER - 1, |n| n.min(FOREVER - 1)),
        };
        self.0.store(raw, Ordering::Relaxed);
    }

    fn clear(&self) {
        self.0.store(UNSET, Ordering::Relaxed);
    }

    /// `None`: unset. `Some(None)`: forever. `Some(Some(d))`: `d`.
    fn get(&self) -> Option<Option<Duration>> {
        match self.0.load(Ordering::Relaxed) {
            UNSET => None,
            FOREVER => Some(None),
            n => Some(Some(Duration::from_nanos(n))),
        }
    }
}

static TTL: Slot = Slot::new();
static MUTEX_TTL: Slot = Slot::new();
static READ_TTL: Slot = Slot::new();
static WRITE_TTL: Slot = Slot::new();
static SEMAPHORE_TTL: Slot = Slot::new();

static WAIT: Slot = Slot::new();
static MUTEX_WAIT: Slot = Slot::new();
static READ_WAIT: Slot = Slot::new();
static WRITE_WAIT: Slot = Slot::new();
static SEMAPHORE_WAIT: Slot = Slot::new();
static EVENT_WAIT: Slot = Slot::new();

static EVENT_PUBLISH_FOR: Slot = Slot::new();

fn ttl_slot(kind: Kind) -> &'static Slot {
    match kind {
        Kind::Mutex => &MUTEX_TTL,
        Kind::Read => &READ_TTL,
        Kind::Write => &WRITE_TTL,
        Kind::Semaphore { .. } => &SEMAPHORE_TTL,
    }
}

fn wait_slot(kind: Kind) -> &'static Slot {
    match kind {
        Kind::Mutex => &MUTEX_WAIT,
        Kind::Read => &READ_WAIT,
        Kind::Write => &WRITE_WAIT,
        Kind::Semaphore { .. } => &SEMAPHORE_WAIT,
    }
}

fn check_ttl(ttl: Duration) -> Result<()> {
    if ttl.is_zero() || ttl > MAX_TTL {
        return Err(Error::InvalidArgument(
            "a default ttl must be greater than zero (and fit in 32 bits of milliseconds)".into(),
        ));
    }
    Ok(())
}

// ------------------------------------------------------------------------------------ resolving

/// The process-wide lease length for `kind`, if one is set (the kind's own, else the general one).
/// `None` means the client's own `default_ttl` applies.
pub fn effective_ttl(kind: Kind) -> Option<Duration> {
    ttl_slot(kind).get().or_else(|| TTL.get()).flatten()
}

/// The process-wide wait for a lock of `kind`, after the cascade. `None`: as long as the server allows.
pub fn effective_wait(kind: Kind) -> Option<Duration> {
    wait_slot(kind).get().or_else(|| WAIT.get()).flatten()
}

/// The process-wide wait for an event, after the cascade. `None`: as long as the server allows.
pub fn effective_event_wait() -> Option<Duration> {
    EVENT_WAIT.get().or_else(|| WAIT.get()).flatten()
}

/// The process-wide publish window, if one is set. `None`: the server's default.
pub fn effective_event_publish_for() -> Option<Duration> {
    EVENT_PUBLISH_FOR.get().flatten()
}

// ------------------------------------------------------------------------------------ setters

macro_rules! ttl_defaults {
    ($($set:ident, $reset:ident, $slot:ident, $what:literal;)*) => {$(
        #[doc = concat!("Set the lease length used by ", $what, " that do not set their own `ttl`.")]
        ///
        /// Fails with [`Error::InvalidArgument`] for zero, or for more than the server can count
        /// (about 49 days).
        pub fn $set(ttl: Duration) -> Result<()> {
            check_ttl(ttl)?;
            $slot.set(Some(ttl));
            Ok(())
        }

        #[doc = concat!("Forget the default set for ", $what, "; the next level of the cascade applies.")]
        pub fn $reset() {
            $slot.clear();
        }
    )*};
}

macro_rules! wait_defaults {
    ($($set:ident, $reset:ident, $slot:ident, $what:literal;)*) => {$(
        #[doc = concat!("Set how long ", $what, " wait when they do not set their own `wait`.")]
        ///
        /// `None` means *as long as the server allows*; `Some(Duration::ZERO)` means do not wait.
        pub fn $set(wait: Option<Duration>) {
            $slot.set(wait);
        }

        #[doc = concat!("Forget the default set for ", $what, "; the next level of the cascade applies.")]
        pub fn $reset() {
            $slot.clear();
        }
    )*};
}

ttl_defaults! {
    set_default_ttl, reset_default_ttl, TTL, "every kind of lock";
    set_default_mutex_ttl, reset_default_mutex_ttl, MUTEX_TTL, "mutexes";
    set_default_read_ttl, reset_default_read_ttl, READ_TTL, "read locks";
    set_default_write_ttl, reset_default_write_ttl, WRITE_TTL, "write locks";
    set_default_semaphore_ttl, reset_default_semaphore_ttl, SEMAPHORE_TTL, "semaphores";
}

wait_defaults! {
    set_default_wait, reset_default_wait, WAIT, "every lock and event";
    set_default_mutex_wait, reset_default_mutex_wait, MUTEX_WAIT, "mutexes";
    set_default_read_wait, reset_default_read_wait, READ_WAIT, "read locks";
    set_default_write_wait, reset_default_write_wait, WRITE_WAIT, "write locks";
    set_default_semaphore_wait, reset_default_semaphore_wait, SEMAPHORE_WAIT, "semaphores";
    set_default_event_wait, reset_default_event_wait, EVENT_WAIT, "event waiters";
}

/// Set the publish window of events that do not set their own `publish_for`: how long after a set a
/// late waiter is still answered at once.
pub fn set_default_event_publish_for(window: Duration) {
    EVENT_PUBLISH_FOR.set(Some(window));
}

/// Forget the default publish window; the server's default applies again.
pub fn reset_default_event_publish_for() {
    EVENT_PUBLISH_FOR.clear();
}

/// Forget every default set in this module.
pub fn reset_all() {
    for slot in [
        &TTL,
        &MUTEX_TTL,
        &READ_TTL,
        &WRITE_TTL,
        &SEMAPHORE_TTL,
        &WAIT,
        &MUTEX_WAIT,
        &READ_WAIT,
        &WRITE_WAIT,
        &SEMAPHORE_WAIT,
        &EVENT_WAIT,
        &EVENT_PUBLISH_FOR,
    ] {
        slot.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The slots are process-wide, so tests that touch them take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_kind_default_beats_the_general_one_and_none_stops_the_cascade() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset_all();
        let sem = Kind::Semaphore {
            capacity: 1,
            permits: 1,
        };
        assert_eq!(effective_wait(sem), None);
        set_default_wait(Some(ms(5)));
        assert_eq!(effective_wait(sem), Some(ms(5)));
        set_default_semaphore_wait(Some(ms(9)));
        assert_eq!(effective_wait(sem), Some(ms(9)));
        assert_eq!(effective_wait(Kind::Mutex), Some(ms(5)));
        set_default_semaphore_wait(None); // forever, explicitly: does not fall through to 5 ms
        assert_eq!(effective_wait(sem), None);
        reset_default_semaphore_wait();
        assert_eq!(effective_wait(sem), Some(ms(5)));
        reset_all();
    }

    #[test]
    fn ttl_defaults_are_validated_and_cascade() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset_all();
        assert!(set_default_ttl(Duration::ZERO).is_err());
        assert!(set_default_ttl(Duration::from_secs(10_000_000)).is_err());
        assert_eq!(effective_ttl(Kind::Mutex), None);
        set_default_ttl(ms(100)).unwrap();
        set_default_write_ttl(ms(200)).unwrap();
        assert_eq!(effective_ttl(Kind::Mutex), Some(ms(100)));
        assert_eq!(effective_ttl(Kind::Write), Some(ms(200)));
        reset_all();
        assert_eq!(effective_ttl(Kind::Write), None);
    }

    #[test]
    fn events_fall_back_to_the_general_wait() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset_all();
        set_default_wait(Some(ms(7)));
        assert_eq!(effective_event_wait(), Some(ms(7)));
        set_default_event_wait(Some(ms(3)));
        assert_eq!(effective_event_wait(), Some(ms(3)));
        assert_eq!(effective_event_publish_for(), None);
        set_default_event_publish_for(ms(40));
        assert_eq!(effective_event_publish_for(), Some(ms(40)));
        reset_all();
    }
}
