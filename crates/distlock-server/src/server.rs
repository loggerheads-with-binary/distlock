//! Listener, connection limit and graceful shutdown.

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use distlock_core::LockTable;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::actor::{Actor, Command};
use crate::config::ServerConfig;
use crate::conn::{self, Ctx};
use crate::users::UserDb;

/// Commands from all connections queue here; a full queue slows readers down (TCP backpressure).
const ENGINE_QUEUE: usize = 8192;
/// How long clients get, after the `Shutdown` notice, before their connections are cut.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(1);

pub struct Server {
    listener: TcpListener,
    ctx: Arc<Ctx>,
    actor: Actor,
    shutdown: CancellationToken,
}

impl Server {
    /// Bind the listening socket and build the engine. Nothing is served until [`run`](Self::run).
    pub async fn bind(cfg: ServerConfig, users: UserDb) -> io::Result<Self> {
        let tls = load_tls(&cfg)?;
        let listener = TcpListener::bind(cfg.bind_addr).await?;
        let mut lock_cfg = cfg.lock.clone();
        // Fencing tokens must keep increasing across restarts of this memory-only server.
        lock_cfg.token_floor = wall_clock_micros();
        let (tx, rx) = mpsc::channel(ENGINE_QUEUE);
        let cfg = Arc::new(cfg);
        let users = Arc::new(users);
        let actor = Actor::new(
            Arc::clone(&cfg),
            LockTable::new(lock_cfg),
            rx,
            Arc::clone(&users),
        );
        let ctx = Arc::new(Ctx {
            cfg,
            users,
            actor: tx,
            next_conn: AtomicU64::new(1),
            tls,
        });
        Ok(Self {
            listener,
            ctx,
            actor,
            shutdown: CancellationToken::new(),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Cancel this token to stop accepting, notify clients, and make [`run`](Self::run) return.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub async fn run(self) -> io::Result<()> {
        let Server {
            listener,
            ctx,
            actor,
            shutdown,
        } = self;
        let engine = tokio::spawn(actor.run());
        let limit = Arc::new(Semaphore::new(ctx.cfg.max_connections));
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) => match Arc::clone(&limit).try_acquire_owned() {
                        Ok(permit) => {
                            tokio::spawn(conn::serve(stream, peer, Arc::clone(&ctx), permit));
                        }
                        Err(_) => {
                            warn!(%peer, "connection limit reached; refusing");
                            drop(stream);
                        }
                    },
                    Err(e) => {
                        // Typically EMFILE/ENFILE: back off instead of spinning.
                        error!("accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
            }
        }
        info!("no longer accepting connections");
        drop(listener);
        let _ = ctx
            .actor
            .send(Command::Shutdown {
                drain: SHUTDOWN_DRAIN,
            })
            .await;
        let _ = engine.await;
        Ok(())
    }
}

/// Build the TLS acceptor when a certificate and key are configured.
fn load_tls(cfg: &ServerConfig) -> io::Result<Option<TlsAcceptor>> {
    let (Some(cert), Some(key)) = (&cfg.tls_cert_file, &cfg.tls_key_file) else {
        return Ok(None);
    };
    let ctx = |what: &str, path: &Path, e: &dyn std::fmt::Display| {
        io::Error::other(format!("TLS {what} {}: {e}", path.display()))
    };
    let certs = CertificateDer::pem_file_iter(cert)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| ctx("certificate", cert, &e))?;
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| ctx("key", key, &e))?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| io::Error::other(format!("TLS certificate and key do not fit: {e}")))?;
    Ok(Some(TlsAcceptor::from(Arc::new(config))))
}
fn wall_clock_micros() -> u64 {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    u64::try_from(micros).unwrap_or(u64::MAX)
}
