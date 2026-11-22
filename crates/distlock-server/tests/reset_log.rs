//! What a reset writes to the server's log. This file holds a single test because it installs the
//! process-wide log subscriber, which the server's tasks (on other threads) write to.
mod common;
use std::io::Write;
use std::sync::{Arc, Mutex};

use common::*;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}

#[tokio::test]
async fn every_reset_logs_the_md5_of_the_file_in_force() {
    let log = Captured::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(log.clone())
            .finish(),
    )
    .unwrap();
    let text = || String::from_utf8(log.0.lock().unwrap().clone()).unwrap();

    let srv = FileServer::start(&["billing"]).await;
    let (mut a, _) = Client::login(srv.addr, "billing").await;

    // Nothing changed: still logged, and said to be unchanged.
    let first = srv.md5();
    expect_reset(a.reset_users().await);
    let line = text()
        .lines()
        .find(|l| l.contains("users reset requested"))
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("no log line for an unchanged reset: {}", text()));
    assert!(line.contains(&format!("users_md5={first}")), "{line}");
    assert!(line.contains("unchanged"), "{line}");

    // A changed file: the line that says what changed carries the new hash.
    srv.write(&[("billing", true), ("newcomer", true)]);
    let second = srv.md5();
    expect_reset(a.reset_users().await);
    let line = text()
        .lines()
        .find(|l| l.contains("users reloaded"))
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("no log line for a changed reset: {}", text()));
    assert!(line.contains(&format!("users_md5={second}")), "{line}");
    assert!(line.contains("newcomer"), "{line}");
}
