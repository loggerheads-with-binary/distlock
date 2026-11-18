//! One TCP connection of the pool: a writer task (requests and keepalive pings) and a reader task
//! (replies and server pushes). Requests are matched to replies by `seq`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use distlock_proto::v1 as pb;
use distlock_proto::v1::{client_frame as cf, server_frame as sf};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::client::Inner;
use crate::error::{Error, Result};

pub(crate) type Reader = FramedRead<OwnedReadHalf, LengthDelimitedCodec>;
pub(crate) type Writer = FramedWrite<OwnedWriteHalf, LengthDelimitedCodec>;

/// How long a reply may take before the connection is considered dead.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const OUTBOUND_QUEUE: usize = 1024;

/// Socket options for every connection, set on both ends.
///
/// The application heartbeat (`Ping`/`Pong`, see `DESIGN.md`) is the primary liveness mechanism.
/// These are the backstop underneath it:
///
/// * `SO_KEEPALIVE`: the kernel probes a peer that has been silent for 30 s, and drops the
///   connection if three probes (10 s apart) go unanswered. It also keeps NAT and firewall
///   mappings alive.
/// * `TCP_USER_TIMEOUT` (Linux): drops a connection whose sent data stays unacknowledged for 30 s.
///   Keepalive never fires while data is waiting for an ACK, so without this the kernel keeps
///   retransmitting into a dead path for about fifteen minutes.
pub(crate) fn tune_socket(stream: &tokio::net::TcpStream) {
    let _ = stream.set_nodelay(true);
    let sock = socket2::SockRef::from(stream);
    let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(30));
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows",
        target_os = "freebsd"
    ))]
    let keepalive = keepalive.with_interval(Duration::from_secs(10));
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
    let keepalive = keepalive.with_retries(3);
    let _ = sock.set_tcp_keepalive(&keepalive);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let _ = sock.set_tcp_user_timeout(Some(Duration::from_secs(30)));
}

pub(crate) struct Conn {
    tx: mpsc::Sender<pb::ClientFrame>,
    next_seq: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<sf::Body>>>,
    closed: CancellationToken,
}

impl Conn {
    /// Take over a handshaken connection and start its tasks.
    pub(crate) fn spawn(
        inner: &Arc<Inner>,
        reader: Reader,
        sink: Writer,
        ping_interval: Duration,
    ) -> Arc<Conn> {
        let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE);
        let conn = Arc::new(Conn {
            tx,
            // The handshake used seq 1.
            next_seq: AtomicU64::new(2),
            pending: Mutex::new(HashMap::new()),
            closed: CancellationToken::new(),
        });
        // Heartbeat. The server asked for a ping at least every `ping_interval` and drops clients
        // that stay silent for twice that. We ping at half of it, and the server answers every ping
        // from its connection task (even when the lock engine is busy), so a whole `ping_interval`
        // without any frame, about two unanswered pings, means the path or the peer is dead: close
        // the connection ourselves rather than wait for the kernel, which can take many minutes to
        // give up on a silent peer. Sockets additionally have TCP keepalive as a backstop.
        let ping = (ping_interval / 2).max(Duration::from_millis(50));
        let dead_after = ping_interval.max(Duration::from_secs(1));
        inner
            .runtime
            .spawn(writer_task(Arc::clone(&conn), sink, rx, ping));
        inner.runtime.spawn(reader_task(
            Arc::clone(inner),
            Arc::clone(&conn),
            reader,
            dead_after,
        ));
        conn
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    pub(crate) async fn closed(&self) {
        self.closed.cancelled().await;
    }

    /// Mark the connection dead and fail everything waiting on it.
    pub(crate) fn close(&self) {
        self.closed.cancel();
        self.pending().clear(); // dropping the senders wakes the callers with `ConnectionLost`
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<sf::Body>>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Send a request and wait for its reply. Any failure to get one means the connection is
    /// unusable: it is closed and `ConnectionLost` returned so the caller can retry elsewhere.
    pub(crate) async fn call(&self, body: cf::Body, timeout: Duration) -> Result<sf::Body> {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();
        self.pending().insert(seq, reply_tx);
        if self.is_closed() {
            self.pending().remove(&seq);
            return Err(Error::ConnectionLost);
        }
        let frame = pb::ClientFrame {
            seq,
            body: Some(body),
        };
        if self.tx.send(frame).await.is_err() {
            self.pending().remove(&seq);
            return Err(Error::ConnectionLost);
        }
        match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(Error::ConnectionLost),
            Err(_) => {
                debug!(seq, "no reply within {timeout:?}; dropping the connection");
                self.pending().remove(&seq);
                self.close();
                Err(Error::ConnectionLost)
            }
        }
    }
}

async fn writer_task(
    conn: Arc<Conn>,
    mut sink: Writer,
    mut rx: mpsc::Receiver<pb::ClientFrame>,
    ping_every: Duration,
) {
    let mut ticker = tokio::time::interval(ping_every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // the first tick is immediate
    loop {
        let frame = tokio::select! {
            () = conn.closed.cancelled() => break,
            f = rx.recv() => match f {
                Some(f) => f,
                None => break,
            },
            _ = ticker.tick() => pb::ClientFrame {
                // Unsolicited Pong replies carry an unknown seq and are ignored by the reader.
                seq: conn.next_seq.fetch_add(1, Ordering::Relaxed),
                body: Some(cf::Body::Ping(pb::Ping { nonce: 0 })),
            },
        };
        if sink.send(Bytes::from(frame.encode_to_vec())).await.is_err() {
            break;
        }
    }
    conn.close();
    let _ = SinkExt::<Bytes>::close(&mut sink).await;
}

async fn reader_task(inner: Arc<Inner>, conn: Arc<Conn>, mut reader: Reader, dead_after: Duration) {
    loop {
        let next = tokio::select! {
            () = conn.closed.cancelled() => break,
            r = tokio::time::timeout(dead_after, reader.next()) => r,
        };
        let bytes = match next {
            Err(_) => {
                debug!("nothing from the server for {dead_after:?}; assuming it is gone");
                break;
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                debug!("read failed: {e}");
                break;
            }
            Ok(Some(Ok(bytes))) => bytes,
        };
        let frame = match pb::ServerFrame::decode(&bytes[..]) {
            Ok(f) => f,
            Err(e) => {
                debug!("undecodable frame from the server: {e}");
                break;
            }
        };
        let Some(body) = frame.body else { continue };
        if frame.seq == 0 {
            inner.on_push(body);
        } else if let Some(waiting) = conn.pending().remove(&frame.seq) {
            let _ = waiting.send(body);
        }
        // else: a Pong for one of our keepalive pings, or a reply nobody waits for any more.
    }
    conn.close();
}
