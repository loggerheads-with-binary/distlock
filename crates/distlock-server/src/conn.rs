//! One TCP connection: framing, handshake, keepalive, and forwarding requests to the engine.
//!
//! Wire format: `u32` big-endian length, then one protobuf `ClientFrame` / `ServerFrame`.
//! A connection has a reader (this task) and a writer task fed by a bounded queue. Only the
//! engine decides *what* to say; this module decides *whether the peer is behaving*.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use distlock_proto::v1 as pb;
use distlock_proto::v1::{client_frame, server_frame};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::actor::{Command, ConnHandle, ConnId, Outbound};
use crate::config::ServerConfig;
use crate::convert::{error_body, frame};
use crate::users::UserDb;

const PROTOCOL_VERSION: u32 = 1;
const OUTBOUND_QUEUE: usize = 1024;
const MAX_CLIENT_ID_BYTES: usize = 128;
const MAX_PASSWORD_BYTES: usize = 1024;
const MAX_LABEL_CHARS: usize = 200;
/// How long a closing connection may keep flushing its last frames (e.g. an error reply).
const FLUSH_GRACE: Duration = Duration::from_secs(2);

/// State shared by every connection task.
pub(crate) struct Ctx {
    pub cfg: Arc<ServerConfig>,
    pub users: Arc<UserDb>,
    pub actor: mpsc::Sender<Command>,
    pub next_conn: AtomicU64,
    pub tls: Option<TlsAcceptor>,
}

trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}
type Reader = FramedRead<ReadHalf<Box<dyn Io>>, LengthDelimitedCodec>;
type Writer = FramedWrite<WriteHalf<Box<dyn Io>>, LengthDelimitedCodec>;

#[derive(Debug)]
enum End {
    Eof,
    IdleTimeout,
    HandshakeTimeout,
    /// We told the peer why (error frame queued) and are closing.
    Refused,
    Io(io::Error),
    /// The engine dropped us (slow consumer) or already stopped.
    Dropped,
}

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

pub(crate) async fn serve(
    stream: TcpStream,
    peer: SocketAddr,
    ctx: Arc<Ctx>,
    _permit: OwnedSemaphorePermit,
) {
    tune_socket(&stream);
    let stream: Box<dyn Io> = match &ctx.tls {
        None => Box::new(stream),
        // The TLS handshake counts against the same budget as the protocol handshake.
        Some(acceptor) => {
            match tokio::time::timeout(ctx.cfg.handshake_timeout, acceptor.accept(stream)).await {
                Ok(Ok(tls)) => Box::new(tls),
                Ok(Err(e)) => return debug!(%peer, error = %e, "TLS handshake failed"),
                Err(_) => return debug!(%peer, "TLS handshake timed out"),
            }
        }
    };
    let codec = LengthDelimitedCodec::builder()
        .big_endian()
        .length_field_length(4)
        .max_frame_length(ctx.cfg.max_frame_bytes)
        .new_codec();
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader: Reader = FramedRead::new(read_half, codec.clone());
    let sink: Writer = FramedWrite::new(write_half, codec);

    let (tx, rx) = mpsc::channel(OUTBOUND_QUEUE);
    let kill = CancellationToken::new();
    let id = ConnId(ctx.next_conn.fetch_add(1, Ordering::Relaxed));
    let conn = ConnHandle {
        id,
        tx,
        kill: kill.clone(),
    };

    let mut writer = tokio::spawn(write_loop(sink, rx, ctx.cfg.idle_connection_timeout));
    let mut writer_done = false;
    let end = tokio::select! {
        end = drive(&mut reader, &conn, &ctx, peer) => end,
        _ = &mut writer => { writer_done = true; End::Dropped }
        () = kill.cancelled() => End::Dropped,
    };
    match &end {
        End::Io(e) => debug!(conn = id.0, %peer, error = %e, "connection closed by i/o error"),
        other => debug!(conn = id.0, %peer, reason = ?other, "connection closed"),
    }

    // Tell the engine, then let the writer drain whatever is queued (an error reply, say).
    let _ = ctx.actor.send(Command::ConnClosed { conn: id }).await;
    drop(conn);
    if !writer_done && !kill.is_cancelled() {
        let _ = tokio::time::timeout(FLUSH_GRACE, &mut writer).await;
    }
    writer.abort();
}

async fn write_loop(mut sink: Writer, mut rx: mpsc::Receiver<Outbound>, write_timeout: Duration) {
    while let Some(out) = rx.recv().await {
        match out {
            Outbound::Frame(message) => {
                let bytes = Bytes::from(message.encode_to_vec());
                match tokio::time::timeout(write_timeout, sink.send(bytes)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        debug!("write failed: {e}");
                        break;
                    }
                    Err(_) => {
                        debug!("write timed out; peer is not reading");
                        break;
                    }
                }
            }
            Outbound::Close => break,
        }
    }
    // Flush and shut down our half so the peer sees a clean EOF.
    let _ = SinkExt::<Bytes>::close(&mut sink).await;
}

async fn drive(reader: &mut Reader, conn: &ConnHandle, ctx: &Ctx, peer: SocketAddr) -> End {
    match tokio::time::timeout(
        ctx.cfg.handshake_timeout,
        handshake(reader, conn, ctx, peer),
    )
    .await
    {
        Err(_) => return End::HandshakeTimeout,
        Ok(Err(end)) => return end,
        Ok(Ok(())) => {}
    }
    steady(reader, conn, ctx).await
}

/// Read one frame. `Err(Ok(msg))`-style distinctions are folded into `End`: an undecodable frame
/// is answered with an error and the connection is closed.
async fn read_frame(reader: &mut Reader, conn: &ConnHandle) -> Result<pb::ClientFrame, End> {
    match reader.next().await {
        None => Err(End::Eof),
        Some(Err(e)) if e.kind() == io::ErrorKind::InvalidData => Err(refuse(
            conn,
            0,
            pb::ErrorCode::InvalidArgument,
            format!("bad frame: {e}"),
        )
        .await),
        Some(Err(e)) => Err(End::Io(e)),
        Some(Ok(bytes)) => match pb::ClientFrame::decode(&bytes[..]) {
            Ok(message) => Ok(message),
            Err(e) => Err(refuse(
                conn,
                0,
                pb::ErrorCode::InvalidArgument,
                format!("undecodable frame: {e}"),
            )
            .await),
        },
    }
}

/// Queue an error reply followed by a close.
async fn refuse(
    conn: &ConnHandle,
    seq: u64,
    code: pb::ErrorCode,
    message: impl Into<String>,
) -> End {
    let _ = conn
        .tx
        .send(Outbound::Frame(frame(seq, error_body(code, message))))
        .await;
    let _ = conn.tx.send(Outbound::Close).await;
    End::Refused
}

async fn handshake(
    reader: &mut Reader,
    conn: &ConnHandle,
    ctx: &Ctx,
    peer: SocketAddr,
) -> Result<(), End> {
    let message = read_frame(reader, conn).await?;
    let seq = message.seq;
    if seq == 0 {
        return Err(refuse(
            conn,
            0,
            pb::ErrorCode::InvalidArgument,
            "seq must be non-zero",
        )
        .await);
    }
    match message.body {
        Some(client_frame::Body::Hello(hello)) => {
            if hello.protocol_version != PROTOCOL_VERSION {
                let msg = format!(
                    "unsupported protocol version {}; this server speaks {PROTOCOL_VERSION}",
                    hello.protocol_version
                );
                return Err(refuse(conn, seq, pb::ErrorCode::UnsupportedVersion, msg).await);
            }
            if hello.client_id.is_empty()
                || hello.client_id.len() > MAX_CLIENT_ID_BYTES
                || hello.password.len() > MAX_PASSWORD_BYTES
            {
                return Err(refuse(
                    conn,
                    seq,
                    pb::ErrorCode::InvalidArgument,
                    "client_id or password out of range",
                )
                .await);
            }
            // The only place bcrypt runs. One attempt per connection: a failure closes it.
            if !ctx.users.verify(&hello.client_id, &hello.password).await {
                warn!(%peer, client_id = ?hello.client_id, "authentication failed");
                return Err(refuse(
                    conn,
                    seq,
                    pb::ErrorCode::Unauthenticated,
                    "invalid credentials",
                )
                .await);
            }
            let label: String = hello
                .instance_label
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_LABEL_CHARS)
                .collect();
            let cmd = Command::Open {
                conn: conn.clone(),
                seq,
                client_id: hello.client_id,
                label,
            };
            ctx.actor.send(cmd).await.map_err(|_| End::Dropped)
        }
        Some(client_frame::Body::Attach(attach)) => {
            let cmd = Command::Attach {
                conn: conn.clone(),
                seq,
                session_id: attach.session_id,
                token: attach.session_token,
            };
            ctx.actor.send(cmd).await.map_err(|_| End::Dropped)
        }
        _ => Err(refuse(
            conn,
            seq,
            pb::ErrorCode::InvalidArgument,
            "the first message must be Hello or Attach",
        )
        .await),
    }
}

async fn steady(reader: &mut Reader, conn: &ConnHandle, ctx: &Ctx) -> End {
    loop {
        let message =
            match tokio::time::timeout(ctx.cfg.idle_connection_timeout, read_frame(reader, conn))
                .await
            {
                Err(_) => return End::IdleTimeout,
                Ok(Ok(message)) => message,
                Ok(Err(end)) => return end,
            };
        let seq = message.seq;
        if seq == 0 {
            return refuse(
                conn,
                0,
                pb::ErrorCode::InvalidArgument,
                "seq must be non-zero",
            )
            .await;
        }
        match message.body {
            Some(client_frame::Body::Ping(ping)) => {
                let pong = frame(
                    seq,
                    server_frame::Body::Pong(pb::Pong { nonce: ping.nonce }),
                );
                if conn.tx.send(Outbound::Frame(pong)).await.is_err() {
                    return End::Dropped;
                }
            }
            Some(client_frame::Body::Hello(_) | client_frame::Body::Attach(_)) => {
                return refuse(
                    conn,
                    seq,
                    pb::ErrorCode::InvalidArgument,
                    "already authenticated",
                )
                .await;
            }
            None => {
                return refuse(conn, seq, pb::ErrorCode::InvalidArgument, "empty message").await;
            }
            Some(client_frame::Body::Maintenance(command)) => {
                if let Some(end) = maintenance(conn, seq, command, ctx).await {
                    return end;
                }
            }
            Some(body) => {
                if ctx
                    .actor
                    .send(Command::Request {
                        conn: conn.id,
                        seq,
                        body,
                    })
                    .await
                    .is_err()
                {
                    return End::Dropped;
                }
            }
        }
    }
}

/// Maintenance commands run here, on the connection's own task and never on the lock engine: reading
/// a file and (when bcrypt costs changed) hashing must not stall every other client's locks.
async fn maintenance(
    conn: &ConnHandle,
    seq: u64,
    command: pb::Maintenance,
    ctx: &Ctx,
) -> Option<End> {
    match command.command {
        Some(pb::maintenance::Command::ResetUsers(_)) => match ctx.users.reload().await {
            Ok(reload) => {
                // The engine ends the sessions of whoever lost access, then replies on this connection.
                let cmd = Command::UsersReset {
                    conn: conn.id,
                    seq,
                    reload,
                };
                if ctx.actor.send(cmd).await.is_err() {
                    return Some(End::Dropped);
                }
            }
            Err(e) => {
                // Details (paths, user names) stay in the server log: any client may send this command.
                error!(error = %e, "reloading the users file failed; the previous users stay in force");
                let message = "the users file could not be loaded; the previous users stay in force \
                               (details are in the server log)";
                let reply = frame(seq, error_body(pb::ErrorCode::MaintenanceFailed, message));
                if conn.tx.send(Outbound::Frame(reply)).await.is_err() {
                    return Some(End::Dropped);
                }
            }
        },
        None => {
            let reply = frame(
                seq,
                error_body(pb::ErrorCode::InvalidArgument, "empty maintenance command"),
            );
            if conn.tx.send(Outbound::Frame(reply)).await.is_err() {
                return Some(End::Dropped);
            }
        }
    }
    None
}
