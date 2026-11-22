//! Test harness: the real server in-process, and a TCP proxy that can be cut or paused.
#![allow(dead_code)]

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use distlock_client::DistLockClient;
use distlock_server::{Server, ServerConfig, UserDb};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

pub struct TestServer {
    pub addr: SocketAddr,
    shutdown: CancellationToken,
    task: JoinHandle<io::Result<()>>,
}

impl TestServer {
    pub async fn start() -> Self {
        Self::start_with("127.0.0.1:0".parse().unwrap(), |_| {}).await
    }

    pub async fn start_with(bind: SocketAddr, tweak: impl FnOnce(&mut ServerConfig)) -> Self {
        Self::start_with_users(
            bind,
            &[
                ("billing", "pw-billing", true),
                ("reports", "pw-reports", true),
            ],
            tweak,
        )
        .await
    }

    pub async fn start_with_users(
        bind: SocketAddr,
        users: &[(&str, &str, bool)],
        tweak: impl FnOnce(&mut ServerConfig),
    ) -> Self {
        Self::start_with_db(bind, UserDb::for_tests(users), tweak).await
    }

    pub async fn start_with_db(
        bind: SocketAddr,
        users: UserDb,
        tweak: impl FnOnce(&mut ServerConfig),
    ) -> Self {
        let mut cfg = ServerConfig {
            bind_addr: bind,
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

    /// Stop the server and wait until it has finished (about the shutdown drain, one second).
    pub async fn stop(self) {
        self.shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(10), self.task).await;
    }

    /// Settings for logging in as one of the test users, straight to the server.
    pub fn client(&self, user: &str) -> DistLockClient {
        client_for(self.addr, user)
    }
}

/// bcrypt cost-10 hashes (the minimum the users file accepts), computed once per password.
fn hash_of(password: &str) -> String {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    cache
        .entry(password.to_owned())
        .or_insert_with(|| bcrypt::hash(password, 10).unwrap())
        .clone()
}

/// A server whose users come from a real file the test can rewrite before asking the server to
/// reload it. Every user's password is `pw-<client_id>`.
pub struct FileServer {
    pub server: TestServer,
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl std::ops::Deref for FileServer {
    type Target = TestServer;
    fn deref(&self) -> &TestServer {
        &self.server
    }
}

impl FileServer {
    pub async fn start(users: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");
        write_users(&path, &users.iter().map(|u| (*u, true)).collect::<Vec<_>>());
        let db = UserDb::load(&path, 4).expect("load users");
        let server = TestServer::start_with_db("127.0.0.1:0".parse().unwrap(), db, |_| {}).await;
        Self {
            server,
            _dir: dir,
            path,
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
    /// Replace the file's contents: `(client_id, enabled)`.
    pub fn write(&self, users: &[(&str, bool)]) {
        write_users(&self.path, users);
    }

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

pub fn client_for(addr: SocketAddr, user: &str) -> DistLockClient {
    DistLockClient {
        client_id: user.into(),
        password: format!("pw-{user}"),
        host: addr.ip().to_string(),
        port: addr.port(),
        namespace: None,
        pool_size: 2,
        default_ttl: Duration::from_secs(30),
        ..DistLockClient::default()
    }
}

/// Run a server on its own runtime thread, for the blocking-client tests.
pub fn start_in_background() -> (tokio::runtime::Runtime, SocketAddr) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let server = rt.block_on(TestServer::start());
    let addr = server.addr;
    std::mem::forget(server); // lives as long as the runtime
    (rt, addr)
}

// ---------------------------------------------------------------------------------------- proxy

struct ProxyState {
    accepted: AtomicUsize,
    paused: AtomicBool,
    /// Connections born before this generation silently swallow traffic (a black hole).
    blackhole: AtomicU64,
    /// If non-zero, every new connection is cut this many milliseconds after it is made.
    flap_ms: AtomicU64,
    /// Cancels one connection each.
    conns: Mutex<Vec<CancellationToken>>,
}

/// A TCP forwarder in front of the server, for simulating bad networks:
/// `cut` / `cut_one` sever connections (RST/FIN), `blackhole` makes existing connections swallow
/// bytes without closing (a partition: nothing arrives, nothing errors), `pause` refuses new
/// connections, `flap` kills every new connection soon after it is made.
pub struct Proxy {
    pub addr: SocketAddr,
    cut: watch::Sender<u64>,
    state: Arc<ProxyState>,
    task: JoinHandle<()>,
}

async fn forward(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    state: Arc<ProxyState>,
    born: u64,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match from.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if state.blackhole.load(Ordering::SeqCst) > born {
                    continue; // swallowed
                }
                if to.write_all(&buf[..n]).await.is_err() {
                    return;
                }
            }
        }
    }
}

impl Proxy {
    pub async fn start(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (cut, _) = watch::channel(0u64);
        let state = Arc::new(ProxyState {
            accepted: AtomicUsize::new(0),
            paused: AtomicBool::new(false),
            blackhole: AtomicU64::new(0),
            flap_ms: AtomicU64::new(0),
            conns: Mutex::new(Vec::new()),
        });
        let task = {
            let cut = cut.clone();
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                loop {
                    let Ok((client, _)) = listener.accept().await else {
                        return;
                    };
                    state.accepted.fetch_add(1, Ordering::SeqCst);
                    if state.paused.load(Ordering::SeqCst) {
                        continue; // dropped: the client sees an immediate close
                    }
                    let mut cut_rx = cut.subscribe();
                    let one = CancellationToken::new();
                    state.conns.lock().unwrap().push(one.clone());
                    let born = state.blackhole.load(Ordering::SeqCst);
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        let Ok(server) = TcpStream::connect(upstream).await else {
                            return;
                        };
                        let (cr, cw) = client.into_split();
                        let (sr, sw) = server.into_split();
                        let flap = match state.flap_ms.load(Ordering::SeqCst) {
                            0 => None,
                            ms => Some(Duration::from_millis(ms)),
                        };
                        tokio::select! {
                            _ = forward(cr, sw, Arc::clone(&state), born) => {}
                            _ = forward(sr, cw, Arc::clone(&state), born) => {}
                            _ = cut_rx.changed() => {}
                            () = one.cancelled() => {}
                            () = async { match flap { Some(d) => tokio::time::sleep(d).await, None => std::future::pending().await } } => {}
                        }
                    });
                }
            })
        };
        Self {
            addr,
            cut,
            state,
            task,
        }
    }

    /// Connections accepted so far (including ones refused while paused).
    pub fn accepted(&self) -> usize {
        self.state.accepted.load(Ordering::SeqCst)
    }

    /// Sever every open connection.
    pub fn cut(&self) {
        self.cut.send_modify(|generation| *generation += 1);
    }

    /// Sever the oldest connection that is still open.
    pub fn cut_one(&self) {
        let conns = self.state.conns.lock().unwrap();
        if let Some(i) = conns.iter().position(|c| !c.is_cancelled()) {
            conns[i].cancel();
        }
    }

    /// Existing connections go silent: bytes are swallowed, nothing is closed.
    pub fn blackhole(&self) {
        self.state.blackhole.fetch_add(1, Ordering::SeqCst);
    }

    /// Refuse (close at once) new connections until `resume`.
    pub fn pause(&self) {
        self.state.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.state.paused.store(false, Ordering::SeqCst);
    }

    /// Cut every *new* connection `after` it is made (zero turns this off).
    pub fn flap(&self, after: Duration) {
        self.state
            .flap_ms
            .store(after.as_millis() as u64, Ordering::SeqCst);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ---------------------------------------------------------------------------- test PKI
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, SanType};
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use tempfile::TempDir;
pub struct Pem {
    pub cert: PathBuf,
    pub key: PathBuf,
}
pub fn names(dns: &str) -> Vec<SanType> {
    vec![
        SanType::DnsName(dns.try_into().unwrap()),
        SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST)),
    ]
}
/// Wrong-name certificates must not name 127.0.0.1 either.
pub fn wrong_names() -> Vec<SanType> {
    vec![SanType::DnsName("wrong.example".try_into().unwrap())]
}
pub struct Pki {
    pub dir: TempDir,
    pub ca_pem: PathBuf,
    ca: Issuer<'static, KeyPair>,
}
impl Pki {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let cert = params.self_signed(&key).unwrap();
        let ca_pem = dir.path().join("ca.pem");
        std::fs::write(&ca_pem, cert.pem()).unwrap();
        Self {
            dir,
            ca_pem,
            ca: Issuer::new(params, key),
        }
    }
    fn write(&self, name: &str, cert: &str, key: &KeyPair) -> Pem {
        let p = Pem {
            cert: self.dir.path().join(format!("{name}.crt")),
            key: self.dir.path().join(format!("{name}.key")),
        };
        std::fs::write(&p.cert, cert).unwrap();
        std::fs::write(&p.key, key.serialize_pem()).unwrap();
        p
    }
    pub fn issued(&self, name: &str, sans: Vec<SanType>) -> Pem {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.subject_alt_names = sans;
        let cert = params.signed_by(&key, &self.ca).unwrap();
        self.write(name, &cert.pem(), &key)
    }
    pub fn self_signed(&self, name: &str, sans: Vec<SanType>) -> Pem {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.subject_alt_names = sans;
        let cert = params.self_signed(&key).unwrap();
        self.write(name, &cert.pem(), &key)
    }
}
pub async fn tls_server(pem: &Pem) -> TestServer {
    let (cert, key) = (pem.cert.clone(), pem.key.clone());
    TestServer::start_with("127.0.0.1:0".parse().unwrap(), move |c| {
        c.tls_enabled = true;
        c.tls_cert_file = Some(cert);
        c.tls_key_file = Some(key);
    })
    .await
}
impl Pki {
    pub fn ca_file(&self) -> PathBuf {
        self.ca_pem.clone()
    }
}
