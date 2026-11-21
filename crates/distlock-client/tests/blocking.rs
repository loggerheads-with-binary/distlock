//! The blocking client. Plain `#[test]`s on plain threads: no async runtime in sight.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::*;
use distlock_client::{Acquire, Error, State};

#[test]
fn acquire_status_and_release_work_without_async() {
    let (_rt, addr) = start_in_background();
    let a = client_for(addr, "billing").connect_blocking().unwrap();
    let b = client_for(addr, "reports").connect_blocking().unwrap();
    assert!(a.session_id().is_some());

    let lock = a.acquire(Acquire::mutex("job")).unwrap();
    assert!(b.try_acquire(Acquire::mutex("job")).unwrap().is_none());
    assert!(matches!(
        b.acquire(Acquire::mutex("job").wait(Duration::from_millis(150))),
        Err(Error::Timeout)
    ));
    let token = lock.fencing_token();
    lock.release_with(Some(State::Stored), None).unwrap();

    let next = b.acquire(Acquire::mutex("job")).unwrap();
    let prev = next.previous().expect("status");
    assert_eq!((prev.state, prev.set_by_token), (State::Stored, token));
    assert!(!next.is_lost());
    next.release().unwrap();
}

#[test]
fn threads_contending_for_a_lock_are_mutually_exclusive() {
    let (_rt, addr) = start_in_background();
    let client = client_for(addr, "billing").connect_blocking().unwrap();
    let counter = Arc::new(AtomicU64::new(0));
    let threads: Vec<_> = (0..6)
        .map(|_| {
            let client = client.clone();
            let counter = Arc::clone(&counter);
            std::thread::spawn(move || {
                for _ in 0..6 {
                    let lock = client.acquire(Acquire::mutex("counter")).unwrap();
                    let seen = counter.load(Ordering::SeqCst);
                    std::thread::yield_now();
                    counter.store(seen + 1, Ordering::SeqCst);
                    lock.release().unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(counter.load(Ordering::SeqCst), 36);
}

#[test]
fn dropping_a_blocking_lock_releases_it() {
    let (_rt, addr) = start_in_background();
    let a = client_for(addr, "billing").connect_blocking().unwrap();
    let b = client_for(addr, "reports").connect_blocking().unwrap();
    drop(a.acquire(Acquire::mutex("m")).unwrap());
    // Released in the background; the next acquire waits for it.
    let got = b
        .acquire(Acquire::mutex("m").wait(Duration::from_secs(3)))
        .unwrap();
    assert!(!got.is_lost());
    a.close().unwrap();
}
#[test]
fn stats_work_without_async() {
    let (_rt, addr) = start_in_background();
    let a = client_for(addr, "billing").connect_blocking().unwrap();
    let s = a.stats().unwrap();
    assert_eq!(s.connected_clients, 1);
    assert!(s.process.unwrap().memory_bytes.is_some_and(|m| m > 0));
}
