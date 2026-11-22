//! `client.stats()`: the health figures a server reports about itself.
mod common;
use common::*;
use distlock_client::Acquire;
use std::time::Duration;
#[tokio::test]
async fn stats_count_clients_and_locks() {
    let srv = TestServer::start().await;
    let a = srv.client("billing").connect().await.unwrap();
    let b = srv.client("reports").connect().await.unwrap();
    let _held = a.acquire(Acquire::mutex("m")).await.unwrap();
    let s = b.stats().await.unwrap();
    assert_eq!(s.connected_clients, 2);
    assert_eq!(s.sessions, 2);
    assert!(s.connections >= 2);
    assert_eq!(s.locks_held, 1);
    assert_eq!(s.requests_queued, 0);
    assert!(!s.in_startup_grace);
    let process = s.process.unwrap();
    assert!(process.memory_bytes.is_some_and(|m| m > 0));
    let system = s.system.unwrap();
    assert!(system.total_memory_bytes > 0);
    assert!(
        (0.0..=100.0).contains(&system.cpu_percent),
        "{}",
        system.cpu_percent
    );
    assert!(
        (0.0..=100.0).contains(&process.cpu_percent),
        "{}",
        process.cpu_percent
    );
}
#[tokio::test]
async fn stats_report_the_startup_grace_period() {
    let srv = TestServer::start_with("127.0.0.1:0".parse().unwrap(), |c| {
        c.lock.startup_grace = Duration::from_secs(60);
    })
    .await;
    let a = srv.client("billing").connect().await.unwrap();
    let s = a.stats().await.unwrap();
    assert!(s.in_startup_grace);
    assert!(s.startup_grace_remaining_ms > 50_000);
}
#[tokio::test]
async fn stats_carry_the_md5_of_the_users_file_and_none_for_users_built_in_memory() {
    let in_memory = TestServer::start().await;
    let s = in_memory
        .client("billing")
        .connect()
        .await
        .unwrap()
        .stats()
        .await
        .unwrap();
    assert_eq!(s.users_md5, "", "no file, no hash");

    let srv = FileServer::start(&["billing"]).await;
    let c = srv.client("billing").connect().await.unwrap();
    let before = c.stats().await.unwrap().users_md5;
    assert_eq!(before.len(), 32, "{before}");
    srv.write(&[("billing", true), ("reports", true)]);
    assert_eq!(c.stats().await.unwrap().users_md5, before, "not read yet");
    let report = c.maintenance().reset_users().await.unwrap();
    let after = c.stats().await.unwrap().users_md5;
    assert_ne!(after, before);
    assert_eq!(report.users_md5, after, "the reset and the stats agree");
}
