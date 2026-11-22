//! Test harness: an in-process server on a loopback port, and a minimal protocol client.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use distlock_proto::v1 as pb;
use distlock_proto::v1::{client_frame as cf, server_frame as sf};
use distlock_server::{Server, ServerConfig, UserDb};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tokio_util::sync::CancellationToken;

/// Generous upper bound for anything that is supposed to happen "promptly".
pub const WAIT: Duration = Duration::from_secs(5);

pub fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

// ---------------------------------------------------------------------------- server

pub struct TestServer {
    pub addr: SocketAddr,
    shutdown: CancellationToken,
    task: JoinHandle<io::Result<()>>,
}

impl TestServer {
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    /// Fast defaults for tests (no lock-delay, no startup grace), then `tweak`.
    pub async fn start_with(tweak: impl FnOnce(&mut ServerConfig)) -> Self {
        let users = UserDb::for_tests(&[
            ("billing", "pw-billing", true),
            ("reports", "pw-reports", true),
            ("retired", "pw-retired", false),
        ]);
        Self::start_with_users(users, tweak).await
    }

    pub async fn start_with_users(users: UserDb, tweak: impl FnOnce(&mut ServerConfig)) -> Self {
        let mut cfg = ServerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            ..ServerConfig::default()
        };
        cfg.lock.lock_delay = Duration::ZERO;
        cfg.lock.startup_grace = Duration::ZERO;
        tweak(&mut cfg);
        let server = Server::bind(cfg, users).await.expect("bind");
        let addr = server.local_addr().expect("local addr");
        let shutdown = server.shutdown_token();
        let task = tokio::spawn(server.run());
        Self {
            addr,
            shutdown,
            task,
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    /// Shut down and wait for `run` to return.
    pub async fn stop(self) {
        self.shutdown.cancel();
        let _ = tokio::time::timeout(WAIT, self.task).await;
    }
}

// ---------------------------------------------------------------------------- file-backed users

/// bcrypt cost-10 hashes (the minimum the users file accepts), computed once per password.
fn hash_of(password: &str) -> String {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    cache
        .entry(password.to_owned())
        .or_insert_with(|| bcrypt::hash(password, 10).unwrap())
        .clone()
}

/// A server whose users come from a real file that the test can rewrite, then ask the server to
/// reload (`ResetUsers`). Every user's password is `pw-<client_id>`.
pub struct FileServer {
    pub server: TestServer,
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl std::ops::Deref for FileServer {
    type Target = TestServer;
    fn deref(&self) -> &TestServer {
        &self.server
    }
}

impl FileServer {
    pub async fn start(users: &[&str]) -> Self {
        Self::start_with(users, |_| {}).await
    }

    pub async fn start_with(users: &[&str], tweak: impl FnOnce(&mut ServerConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");
        write_users(&path, &users.iter().map(|u| (*u, true)).collect::<Vec<_>>());
        let db = UserDb::load(&path, 4).expect("load users");
        let server = TestServer::start_with_users(db, tweak).await;
        Self {
            server,
            _dir: dir,
            path,
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// MD5 of the file as it is on disk now, as 32 lowercase hex characters.
    pub fn md5(&self) -> String {
        use md5::{Digest, Md5};
        Md5::digest(std::fs::read(&self.path).unwrap())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Replace the file's contents: `(client_id, enabled)`, password `pw-<client_id>`.
    pub fn write(&self, users: &[(&str, bool)]) {
        write_users(&self.path, users);
    }

    /// Replace the file's contents with an explicit list of `(client_id, password, enabled)`.
    pub fn write_with_passwords(&self, users: &[(&str, &str, bool)]) {
        let entries: Vec<_> = users
            .iter()
            .map(|(id, pw, enabled)| serde_json::json!({"client_id": id, "password_hash": hash_of(pw), "enabled": enabled}))
            .collect();
        let doc = serde_json::json!({"version": 1, "users": entries});
        std::fs::write(&self.path, doc.to_string()).unwrap();
    }

    /// Replace the file's contents with arbitrary text.
    pub fn write_raw(&self, text: &str) {
        std::fs::write(&self.path, text).unwrap();
    }
}

fn write_users(path: &std::path::Path, users: &[(&str, bool)]) {
    let entries: Vec<_> = users
        .iter()
        .map(|(id, enabled)| {
            serde_json::json!({
                "client_id": id,
                "password_hash": hash_of(&format!("pw-{id}")),
                "enabled": enabled,
            })
        })
        .collect();
    let doc = serde_json::json!({"version": 1, "users": entries});
    std::fs::write(path, doc.to_string()).unwrap();
}

// ---------------------------------------------------------------------------- client

pub enum Recv {
    Frame(pb::ServerFrame),
    Closed,
    Timeout,
}

pub struct Client {
    framed: Framed<TcpStream, LengthDelimitedCodec>,
    seq: u64,
    /// Unsolicited (seq 0) messages that arrived while waiting for a reply.
    pub pushes: VecDeque<sf::Body>,
}

pub fn hello_body(user: &str, password: &str) -> cf::Body {
    cf::Body::Hello(pb::Hello {
        protocol_version: 1,
        client_id: user.into(),
        password: password.into(),
        instance_label: "e2e-test".into(),
    })
}

pub fn attach_body(session_id: u64, token: &[u8]) -> cf::Body {
    cf::Body::Attach(pb::Attach {
        session_id,
        session_token: token.to_vec(),
    })
}

impl Client {
    pub async fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).await.expect("connect");
        let codec = LengthDelimitedCodec::builder()
            .big_endian()
            .length_field_length(4)
            .max_frame_length(8 << 20)
            .new_codec();
        Self {
            framed: Framed::new(stream, codec),
            seq: 0,
            pushes: VecDeque::new(),
        }
    }

    /// Log in as one of the test users and return the connection and the `HelloOk`.
    pub async fn login(addr: SocketAddr, user: &str) -> (Self, pb::HelloOk) {
        let password = format!("pw-{user}");
        let mut c = Self::connect(addr).await;
        match c.call(hello_body(user, &password)).await {
            sf::Body::HelloOk(ok) => (c, ok),
            other => panic!("expected HelloOk, got {other:?}"),
        }
    }

    /// Join an existing session on a new connection.
    pub async fn attach(addr: SocketAddr, hello: &pb::HelloOk) -> (Self, pb::AttachOk) {
        let mut c = Self::connect(addr).await;
        match c
            .call(attach_body(hello.session_id, &hello.session_token))
            .await
        {
            sf::Body::AttachOk(ok) => (c, ok),
            other => panic!("expected AttachOk, got {other:?}"),
        }
    }

    pub async fn send(&mut self, body: cf::Body) -> u64 {
        self.seq += 1;
        let seq = self.seq;
        let bytes = pb::ClientFrame {
            seq,
            body: Some(body),
        }
        .encode_to_vec();
        self.framed.send(Bytes::from(bytes)).await.expect("send");
        seq
    }

    /// Fire a try-lock request without waiting for (or reading) any reply.
    pub async fn try_send_acquire(&mut self, request_id: u64, name: &str) -> io::Result<()> {
        self.seq += 1;
        let acquire = pb::Acquire {
            request_id,
            name: name.into(),
            mode: pb::Mode::Mutex as i32,
            ttl_ms: 1_000,
            ..Default::default()
        };
        let bytes = pb::ClientFrame {
            seq: self.seq,
            body: Some(cf::Body::Acquire(acquire)),
        }
        .encode_to_vec();
        self.framed.send(Bytes::from(bytes)).await
    }

    /// Send a frame with an explicit `seq`, bypassing the counter.
    pub async fn send_with_seq(&mut self, seq: u64, body: cf::Body) {
        let bytes = pb::ClientFrame {
            seq,
            body: Some(body),
        }
        .encode_to_vec();
        self.framed.send(Bytes::from(bytes)).await.expect("send");
    }

    /// Send arbitrary bytes as one length-prefixed frame.
    pub async fn send_raw(&mut self, payload: Vec<u8>) {
        self.framed.send(Bytes::from(payload)).await.expect("send");
    }

    pub async fn recv(&mut self, within: Duration) -> Recv {
        match tokio::time::timeout(within, self.framed.next()).await {
            Err(_) => Recv::Timeout,
            Ok(None | Some(Err(_))) => Recv::Closed,
            Ok(Some(Ok(bytes))) => Recv::Frame(
                pb::ServerFrame::decode(&bytes[..]).expect("server sent an undecodable frame"),
            ),
        }
    }

    /// Send a request and wait for its reply; unsolicited messages are set aside in `pushes`.
    pub async fn call(&mut self, body: cf::Body) -> sf::Body {
        let seq = self.send(body).await;
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.recv(left).await {
                Recv::Frame(f) if f.seq == seq => return f.body.expect("reply without a body"),
                Recv::Frame(f) if f.seq == 0 => {
                    self.pushes.push_back(f.body.expect("push without a body"))
                }
                Recv::Frame(f) => panic!("reply with unexpected seq {} (waiting for {seq})", f.seq),
                Recv::Closed => panic!("connection closed while waiting for a reply"),
                Recv::Timeout => panic!("timed out waiting for a reply"),
            }
        }
    }

    pub async fn next_push(&mut self, within: Duration) -> Option<sf::Body> {
        if let Some(body) = self.pushes.pop_front() {
            return Some(body);
        }
        match self.recv(within).await {
            Recv::Frame(f) => {
                assert_eq!(f.seq, 0, "expected an unsolicited message, got a reply");
                f.body
            }
            Recv::Closed | Recv::Timeout => None,
        }
    }

    pub async fn expect_push(&mut self) -> sf::Body {
        self.next_push(WAIT)
            .await
            .expect("expected a push, got none")
    }

    pub async fn expect_no_push(&mut self, within: Duration) {
        if let Some(body) = self.next_push(within).await {
            panic!("unexpected push: {body:?}");
        }
    }

    /// Drain until the server closes the connection.
    pub async fn expect_closed(&mut self) {
        loop {
            match self.recv(WAIT).await {
                Recv::Closed => return,
                Recv::Frame(_) => {}
                Recv::Timeout => panic!("connection is still open"),
            }
        }
    }

    // ------------------------------------------------------------------ request helpers

    pub async fn acquire(
        &mut self,
        request_id: u64,
        name: &str,
        mode: pb::Mode,
        ttl_ms: u32,
        wait_ms: u32,
    ) -> sf::Body {
        self.call(cf::Body::Acquire(pb::Acquire {
            request_id,
            name: name.into(),
            mode: mode as i32,
            ttl_ms,
            wait_ms,
            semaphore_capacity: 0,
            permits: 0,
        }))
        .await
    }

    pub async fn acquire_semaphore(
        &mut self,
        request_id: u64,
        name: &str,
        capacity: u32,
        permits: u32,
        ttl_ms: u32,
        wait_ms: u32,
    ) -> sf::Body {
        self.call(cf::Body::Acquire(pb::Acquire {
            request_id,
            name: name.into(),
            mode: pb::Mode::Semaphore as i32,
            ttl_ms,
            wait_ms,
            semaphore_capacity: capacity,
            permits,
        }))
        .await
    }

    pub async fn release(&mut self, name: &str, token: u64, state: pb::HolderState) -> sf::Body {
        self.call(cf::Body::Release(pb::Release {
            name: name.into(),
            fencing_token: token,
            state: state as i32,
            state_ttl_ms: 0,
        }))
        .await
    }

    pub async fn extend(&mut self, name: &str, token: u64, ttl_ms: u32) -> pb::ExtendResult {
        let body = self
            .call(cf::Body::Extend(pb::Extend {
                items: vec![pb::ExtendItem {
                    name: name.into(),
                    fencing_token: token,
                    ttl_ms,
                }],
            }))
            .await;
        match body {
            sf::Body::ExtendOk(mut ok) => ok.results.remove(0),
            other => panic!("expected ExtendOk, got {other:?}"),
        }
    }

    /// Ask the server to reload its users file.
    pub async fn reset_users(&mut self) -> sf::Body {
        self.call(cf::Body::Maintenance(pb::Maintenance {
            command: Some(pb::maintenance::Command::ResetUsers(pb::ResetUsers {})),
        }))
        .await
    }

    /// Wait for an event (`after_id` 0: any publication counts).
    pub async fn event_wait(
        &mut self,
        request_id: u64,
        name: &str,
        wait_ms: u32,
        after_id: u64,
    ) -> sf::Body {
        self.call(cf::Body::EventWait(pb::EventWait {
            request_id,
            name: name.into(),
            wait_ms,
            after_id,
        }))
        .await
    }

    /// Set an event. `publish_ms: None` asks for the server's default window.
    pub async fn event_set(
        &mut self,
        name: &str,
        value: Option<&str>,
        publish_ms: Option<u32>,
    ) -> sf::Body {
        self.call(cf::Body::EventSet(pb::EventSet {
            name: name.into(),
            value: value.map(str::to_owned),
            publish_ms,
        }))
        .await
    }

    pub async fn cancel(&mut self, request_id: u64) -> pb::cancel_ok::Outcome {
        match self
            .call(cf::Body::CancelAcquire(pb::CancelAcquire { request_id }))
            .await
        {
            sf::Body::CancelOk(ok) => {
                pb::cancel_ok::Outcome::try_from(ok.outcome).expect("known outcome")
            }
            other => panic!("expected CancelOk, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------- reply inspection

pub fn expect_granted(body: sf::Body) -> pb::Grant {
    match body {
        sf::Body::AcquireOk(pb::AcquireOk {
            outcome: Some(pb::acquire_ok::Outcome::Granted(g)),
        }) => g,
        other => panic!("expected an immediate grant, got {other:?}"),
    }
}

pub fn expect_queued(body: sf::Body) {
    match body {
        sf::Body::AcquireOk(pb::AcquireOk {
            outcome: Some(pb::acquire_ok::Outcome::Queued(_)),
        }) => {}
        other => panic!("expected Queued, got {other:?}"),
    }
}

pub fn expect_error(body: sf::Body) -> pb::Error {
    match body {
        sf::Body::Error(e) => e,
        other => panic!("expected an error, got {other:?}"),
    }
}

pub fn expect_error_code(body: sf::Body, code: pb::ErrorCode) -> pb::Error {
    let e = expect_error(body);
    assert_eq!(
        e.code, code as i32,
        "wrong error code (message: {})",
        e.message
    );
    e
}

pub fn expect_reset(body: sf::Body) -> pb::ResetUsersOk {
    match body {
        sf::Body::MaintenanceOk(pb::MaintenanceOk {
            outcome: Some(pb::maintenance_ok::Outcome::ResetUsers(r)),
        }) => r,
        other => panic!("expected ResetUsersOk, got {other:?}"),
    }
}

pub fn expect_ok_release(body: sf::Body) {
    match body {
        sf::Body::ReleaseOk(_) => {}
        other => panic!("expected ReleaseOk, got {other:?}"),
    }
}

/// A `Granted` push, unwrapped.
pub fn expect_granted_push(body: sf::Body) -> pb::Grant {
    match body {
        sf::Body::Granted(pb::Granted { grant: Some(g) }) => g,
        other => panic!("expected a Granted push, got {other:?}"),
    }
}

pub fn expect_event_queued(body: sf::Body) {
    match body {
        sf::Body::EventWaitOk(pb::EventWaitOk {
            outcome: Some(pb::event_wait_ok::Outcome::Queued(_)),
        }) => {}
        other => panic!("expected a queued event wait, got {other:?}"),
    }
}

pub fn expect_event_published(body: sf::Body) -> pb::Publication {
    match body {
        sf::Body::EventWaitOk(pb::EventWaitOk {
            outcome: Some(pb::event_wait_ok::Outcome::Published(p)),
        }) => p,
        other => panic!("expected an immediate publication, got {other:?}"),
    }
}

pub fn expect_event_set(body: sf::Body) -> pb::EventSetOk {
    match body {
        sf::Body::EventSetOk(ok) => ok,
        other => panic!("expected EventSetOk, got {other:?}"),
    }
}

/// An `EventPublished` push, unwrapped to `(request_id, name, publication)`.
pub fn expect_published_push(body: sf::Body) -> (u64, String, pb::Publication) {
    match body {
        sf::Body::EventPublished(pb::EventPublished {
            request_id,
            name,
            publication: Some(p),
        }) => (request_id, name, p),
        other => panic!("expected an EventPublished push, got {other:?}"),
    }
}
