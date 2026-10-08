//! Socket lifecycle: home dir, single-instance lock, stale-socket cleanup, accept
//! loop, per-connection handshake and request loop, graceful shutdown.

use crate::{BUS_CAPACITY, Conn, DaemonConfig, DaemonError, Paths, Shared, actor, auth, dispatch};
use agora_proto::{
    ClientHello, ErrorBody, ErrorCode, Event, MAX_FRAME, PROTO_VERSION, ProtoError, Req, Request,
    ServerFrame, ServerHello, Welcome, b64, read_frame, write_json,
};
use agora_store::{Store, hex};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, mpsc};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

/// A reply queued for the connection's writer, with the in-flight permit it holds.
/// The permit is released only after the frame has been written (or abandoned).
type Outgoing = (ServerFrame, Option<OwnedSemaphorePermit>);

/// Grace period for connection tasks before they are aborted during shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Longest a single frame write may take before the connection is given up.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest ClientHello accepted; a hello is a few hundred bytes.
const HELLO_MAX_FRAME: usize = 4096;

/// Pause after a failed accept, so a persistent error (e.g. EMFILE) does not spin.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// `sun_path` holds at most 104 bytes on macOS (108 on Linux); longer paths cannot bind.
const MAX_SOCKET_PATH_BYTES: usize = 104;

/// Longest a rejection frame may take to write; a peer that stopped reading must not
/// hold a dying connection open.
const REJECT_TIMEOUT: Duration = Duration::from_secs(1);

pub struct Daemon {
    paths: Paths,
    /// Held (flock) for the daemon's lifetime; never removed.
    lock: std::fs::File,
    listener: UnixListener,
    shared: Shared,
    writer_thread: std::thread::JoinHandle<()>,
    reader_thread: std::thread::JoinHandle<()>,
    token: CancellationToken,
    /// Created in `bind` so a failure there happens before any filesystem change.
    /// `None` when `handle_signals` is off.
    sigint: Option<Signal>,
    sigterm: Option<Signal>,
    max_connections: usize,
}

impl Daemon {
    /// Must run inside a tokio runtime. Steps, in order:
    /// 1. create `home` (and parents) if missing, mode 0700; a symlink at `home` or a
    ///    socket path of 104 bytes or more is a Config error; `home` must be owned by
    ///    the daemon uid (else Config error); chmod 0700.
    /// 2. open/create `paths.lock` (0600) and `File::try_lock()` (std, exclusive,
    ///    non-blocking); WouldBlock -> `AlreadyRunning(lock path)`. Keep the File for
    ///    the daemon's lifetime (never delete the lock file).
    /// 3. holding the lock, any existing `paths.socket` is stale: if it is a socket,
    ///    remove it; if it is anything else -> Config error.
    /// 4. writer Store = `Store::open_with_blob_dir(db, blobs)`, then reader Store the
    ///    same way; spawn both actors; bus = broadcast::channel(BUS_CAPACITY).
    /// 5. bind the UnixListener at `paths.socket`, chmod 0600.
    ///    Log "listening" with the socket path at info.
    pub async fn bind(config: DaemonConfig) -> Result<Daemon, DaemonError> {
        // A directly built config can bypass the loader's checks; reject it here.
        config.validate()?;
        let paths = config.paths();
        // Checked before anything is created: a path that cannot bind must not leave
        // a half-made home behind.
        if paths.socket.as_os_str().as_encoded_bytes().len() >= MAX_SOCKET_PATH_BYTES {
            return Err(DaemonError::Config(format!(
                "socket path too long: {}",
                paths.socket.display()
            )));
        }
        // Signal streams are registered before the home, lock, socket or stores are
        // touched, so a failure here leaves nothing behind.
        let (sigint, sigterm) = if config.handle_signals {
            (
                Some(signal(SignalKind::interrupt())?),
                Some(signal(SignalKind::terminate())?),
            )
        } else {
            (None, None)
        };
        let daemon_uid = auth::daemon_uid();

        // A symlinked home could point the daemon's files anywhere; refuse it.
        match std::fs::symlink_metadata(&paths.home) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(DaemonError::Config(format!(
                    "home {} is a symlink; refusing to use it",
                    paths.home.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(DaemonError::Io(e)),
        }
        // The mode applies only when this call creates the directory.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&paths.home)?;
        let owner = std::fs::metadata(&paths.home)?.uid();
        if owner != daemon_uid {
            return Err(DaemonError::Config(format!(
                "home {} is owned by uid {owner}, not by the daemon uid {daemon_uid}",
                paths.home.display()
            )));
        }
        std::fs::set_permissions(&paths.home, std::fs::Permissions::from_mode(0o700))?;

        let lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&paths.lock)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(DaemonError::AlreadyRunning(paths.lock.clone()));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(DaemonError::Io(e)),
        }

        match std::fs::symlink_metadata(&paths.socket) {
            Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(&paths.socket)?,
            Ok(_) => {
                return Err(DaemonError::Config(format!(
                    "{} exists and is not a socket",
                    paths.socket.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(DaemonError::Io(e)),
        }

        let writer_store = Store::open_with_blob_dir(&paths.db, &paths.blobs)?;
        let reader_store = Store::open_with_blob_dir(&paths.db, &paths.blobs)?;
        let bus = broadcast::channel::<Event>(BUS_CAPACITY).0;
        let (writer, writer_thread) = actor::Writer::spawn(writer_store, bus.clone());
        let (reader, reader_thread) = actor::Reader::spawn(reader_store);

        let listener = UnixListener::bind(&paths.socket)?;
        std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))?;
        tracing::info!(socket = %paths.socket.display(), "listening");

        let token = CancellationToken::new();
        let max_connections = config.max_connections;
        let shared = Shared {
            writer,
            reader,
            bus,
            config: Arc::new(config),
            daemon_uid,
            shutdown: token.clone(),
        };
        Ok(Daemon {
            paths,
            lock,
            listener,
            shared,
            writer_thread,
            reader_thread,
            token,
            sigint,
            sigterm,
            max_connections,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.paths.socket
    }

    /// Cancelling this token stops the daemon (same as a Shutdown request).
    pub fn shutdown_token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// For tests: the shared state (actors, bus, config).
    pub fn shared(&self) -> &Shared {
        &self.shared
    }

    /// Accept loop until the token is cancelled (or SIGINT/SIGTERM when
    /// `config.handle_signals`). Each accepted stream gets its own task running
    /// `serve_conn`; connections above `max_connections` are closed at once.
    /// Shutdown: cancel the token, drop the listener and remove the socket file at
    /// once, wait up to 2 s for connection tasks then abort them, drop every Shared
    /// clone, join the actor threads (via spawn_blocking), drop the lock. Logs "stopped".
    pub async fn run(self) -> Result<(), DaemonError> {
        let Daemon {
            paths,
            lock,
            listener,
            shared,
            writer_thread,
            reader_thread,
            token,
            mut sigint,
            mut sigterm,
            max_connections,
        } = self;

        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    // Reap first, so the limit counts only connections still open.
                    reap(&mut connections);
                    match accepted {
                        Ok((stream, _)) if connections.len() < max_connections => {
                            connections.spawn(serve_conn(shared.clone(), stream));
                        }
                        Ok((_stream, _)) => {
                            tracing::warn!(max = max_connections, "connection limit reached; closing new connection");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "accept failed; retrying");
                            tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                        }
                    }
                }
                _ = token.cancelled() => break,
                _ = recv_signal(&mut sigint) => {
                    tracing::info!("SIGINT received");
                    break;
                }
                _ = recv_signal(&mut sigterm) => {
                    tracing::info!("SIGTERM received");
                    break;
                }
            }
        }

        token.cancel();
        // Stop accepting and free the socket path at once: new clients get "no daemon"
        // while the open connections drain, instead of a socket that accepts and hangs.
        drop(listener);
        remove_socket_file(&paths.socket);

        let grace = tokio::time::timeout(SHUTDOWN_GRACE, async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        if grace.is_err() {
            tracing::warn!("connections still open after the grace period; aborting them");
        }
        connections.abort_all();
        while connections.join_next().await.is_some() {}

        // Every Shared clone must go before the actor threads can finish: they end
        // when the last Writer/Reader handle is dropped.
        drop(shared);
        for thread in [writer_thread, reader_thread] {
            match tokio::task::spawn_blocking(move || thread.join()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => tracing::error!("store actor thread panicked"),
                Err(e) => tracing::error!(error = %e, "could not wait for a store actor thread"),
            }
        }

        // The lock is released last, so no second daemon can open the store while
        // this one still has it.
        drop(lock);
        tracing::info!("stopped");
        Ok(())
    }
}

/// Resolves on the next delivery of `sig`; never resolves when signals are disabled
/// or the stream has ended (so the select branch is simply inert).
async fn recv_signal(sig: &mut Option<Signal>) {
    match sig {
        Some(s) => {
            if s.recv().await.is_none() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

/// Serve one connection (see SPEC "Connection protocol"). Never returns an error:
/// every failure is logged at debug/warn and ends only this connection.
pub async fn serve_conn(sh: Shared, mut stream: UnixStream) {
    let uid = match auth::peer_uid(&stream) {
        Ok(uid) if uid == sh.daemon_uid => uid,
        peer => {
            tracing::warn!(peer_uid = ?peer.ok(), "peer uid mismatch; closing connection");
            reject(&mut stream, "peer uid mismatch").await;
            return;
        }
    };

    // The whole handshake shares one deadline, so a peer that stalls at any step is cut off.
    let timeout = Duration::from_millis(sh.config.handshake_timeout_ms);
    let conn = match tokio::time::timeout(timeout, handshake(&mut stream, uid)).await {
        Ok(Some(conn)) => conn,
        // The handshake already reported its failure to the peer.
        Ok(None) => return,
        Err(_) => {
            tracing::debug!("handshake timed out");
            reject(&mut stream, "handshake timed out").await;
            return;
        }
    };

    let (rd, wr) = stream.into_split();
    // A limit of 0 would make every request wait forever; treat it as 1.
    let window = sh.config.max_inflight_per_conn.max(1);
    let sem = Arc::new(Semaphore::new(window));
    let (tx, rx) = mpsc::channel::<Outgoing>(window);
    // Parsed requests from the reader to the dispatcher. One slot: the reader stays
    // at most one request ahead of the window.
    let (req_tx, mut req_rx) = mpsc::channel::<Request>(1);
    // Cancelled when this connection ends for any reason, so request tasks still in
    // flight stop (an inbox wait must not outlive its client).
    let ending = CancellationToken::new();
    // Every task this connection starts lives in `tasks`, so dropping it (when the
    // daemon aborts the connection) aborts all of them.
    let mut tasks = JoinSet::new();
    tasks.spawn(write_loop(wr, rx));
    // Reads even while the window is full, so a client that closes is noticed at once.
    tasks.spawn(read_requests(
        sh.clone(),
        rd,
        tx.clone(),
        req_tx,
        ending.clone(),
    ));

    loop {
        reap(&mut tasks);
        let req = tokio::select! {
            _ = sh.shutdown.cancelled() => break,
            _ = ending.cancelled() => break,
            _ = tx.closed() => break,
            req = req_rx.recv() => match req {
                Some(req) => req,
                // The reader has ended.
                None => break,
            },
        };
        let permit = tokio::select! {
            _ = sh.shutdown.cancelled() => break,
            _ = ending.cancelled() => break,
            _ = tx.closed() => break,
            permit = Arc::clone(&sem).acquire_owned() => match permit {
                Ok(permit) => permit,
                // The semaphore is never closed; treat it as the end of the connection.
                Err(_) => break,
            },
        };
        let (sh_req, conn_req, reply_to, ending_req) =
            (sh.clone(), conn.clone(), tx.clone(), ending.clone());
        let id = req.id;
        tasks.spawn(async move {
            let frame = tokio::select! {
                _ = ending_req.cancelled() => return,
                frame = answer(sh_req, conn_req, id, req.req) => frame,
            };
            tokio::select! {
                _ = ending_req.cancelled() => {}
                _ = reply_to.send((frame, Some(permit))) => {}
            }
        });
    }

    // Stop the request tasks (their handlers are aborted with them) and the reader, then
    // drop our sender: the writer ends once it has flushed what is queued, or on a
    // write error.
    ending.cancel();
    drop(req_rx);
    drop(tx);
    while let Some(joined) = tasks.join_next().await {
        if let Err(e) = joined {
            tracing::error!(error = %e, "connection task failed");
        }
    }
    tracing::debug!(pseudonym = %agora_crypto::pseudonym(&conn.pubkey), "connection closed");
}

/// Reads frames until the client closes, a read fails, the daemon shuts down or the
/// connection ends, and hands each parsed request to the dispatcher. A malformed frame
/// is answered here, without a permit. Cancels `ending` on every exit, so a client
/// that closes while the window is full stops its in-flight requests at once.
async fn read_requests(
    sh: Shared,
    mut rd: OwnedReadHalf,
    tx: mpsc::Sender<Outgoing>,
    requests: mpsc::Sender<Request>,
    ending: CancellationToken,
) {
    loop {
        let read = tokio::select! {
            _ = ending.cancelled() => break,
            _ = sh.shutdown.cancelled() => break,
            read = read_frame(&mut rd, MAX_FRAME) => read,
        };
        let bytes = match read {
            Ok(Some(bytes)) => bytes,
            // Client closed its side.
            Ok(None) => break,
            Err(ProtoError::FrameTooLarge(len)) => {
                tracing::debug!(len, "frame too large; closing connection");
                send_unpermitted(
                    &sh,
                    &tx,
                    &ending,
                    err_frame(0, ErrorCode::TooLarge, "frame exceeds 8 MiB"),
                )
                .await;
                break;
            }
            Err(e) => {
                tracing::debug!(error = %e, "read failed; closing connection");
                break;
            }
        };

        let req = match serde_json::from_slice::<Request>(&bytes) {
            Ok(req) => req,
            Err(e) => {
                let frame = malformed_frame(&bytes, &e);
                if !send_unpermitted(&sh, &tx, &ending, frame).await {
                    break;
                }
                continue;
            }
        };
        let queued = tokio::select! {
            _ = ending.cancelled() => false,
            _ = sh.shutdown.cancelled() => false,
            sent = requests.send(req) => sent.is_ok(),
        };
        if !queued {
            break;
        }
    }
    ending.cancel();
}

/// The BadRequest answer to a frame that is not a valid request. Only a malformed
/// request pays for a second parse, to recover its id.
fn malformed_frame(bytes: &[u8], e: &serde_json::Error) -> ServerFrame {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Err(_) => err_frame(0, ErrorCode::BadRequest, "malformed JSON"),
        Ok(value) => {
            let id = value
                .get("id")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            err_frame(
                id,
                ErrorCode::BadRequest,
                format!("unknown or malformed request: {e}"),
            )
        }
    }
}

/// Queues an answer that holds no in-flight permit (its request never got one). False
/// when the daemon is shutting down, the connection is ending, or the writer is gone.
async fn send_unpermitted(
    sh: &Shared,
    tx: &mpsc::Sender<Outgoing>,
    ending: &CancellationToken,
    frame: ServerFrame,
) -> bool {
    tokio::select! {
        _ = sh.shutdown.cancelled() => false,
        _ = ending.cancelled() => false,
        sent = tx.send((frame, None)) => sent.is_ok(),
    }
}

/// Drops finished tasks from `tasks` without waiting, logging any that panicked.
fn reap(tasks: &mut JoinSet<()>) {
    while let Some(joined) = tasks.try_join_next() {
        if let Err(e) = joined {
            tracing::error!(error = %e, "connection task failed");
        }
    }
}

/// Steps 2 and 3 of the connection protocol: send the challenge, read and verify the
/// signed reply, send Welcome. The caller bounds the whole exchange with the handshake
/// timeout. `None` ends the connection; any failure has already been reported to the
/// peer where that was possible.
async fn handshake(stream: &mut UnixStream, uid: u32) -> Option<Conn> {
    let challenge = auth::new_challenge();
    let hello = ServerFrame::Hello(ServerHello {
        version: PROTO_VERSION,
        challenge: b64(&challenge),
        daemon: format!("agora/{}", env!("CARGO_PKG_VERSION")),
    });
    write_json(stream, &hello).await.ok()?;

    let bytes = match read_frame(stream, HELLO_MAX_FRAME).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            tracing::debug!("handshake rejected: no hello frame");
            reject(stream, "handshake failed: no hello frame").await;
            return None;
        }
        Err(ProtoError::FrameTooLarge(len)) => {
            tracing::debug!(len, "handshake rejected: hello too large");
            reject(
                stream,
                &format!("handshake failed: hello exceeds {HELLO_MAX_FRAME} bytes"),
            )
            .await;
            return None;
        }
        Err(e) => {
            tracing::debug!(error = %e, "handshake rejected: read failed");
            reject(stream, "handshake failed: read error").await;
            return None;
        }
    };
    let client: ClientHello = match serde_json::from_slice(&bytes) {
        Ok(client) => client,
        Err(_) => {
            tracing::debug!("handshake rejected: malformed hello");
            reject(stream, "handshake failed: malformed hello").await;
            return None;
        }
    };

    let pubkey = match auth::verify_hello(&challenge, &client) {
        Ok(pubkey) => pubkey,
        Err(body) => {
            tracing::debug!(reason = %body.message, "handshake rejected");
            reject(stream, &body.message).await;
            return None;
        }
    };
    let welcome = ServerFrame::Welcome(Welcome {
        pubkey: hex(&pubkey),
        pseudonym: agora_crypto::pseudonym(&pubkey),
    });
    write_json(stream, &welcome).await.ok()?;
    tracing::debug!(pseudonym = %agora_crypto::pseudonym(&pubkey), "handshake ok");

    Some(Conn {
        pubkey,
        uid,
        label: auth::clean_label(&client.label),
    })
}

/// Runs one request handler in its own task, so a panicking handler is answered with
/// an Internal error instead of leaving the client waiting on an id that never returns.
/// Dropping this future (the request task was cancelled) aborts the handler too.
async fn answer(sh: Shared, conn: Conn, id: u64, req: Req) -> ServerFrame {
    let mut handler = AbortOnDrop(tokio::spawn(async move {
        dispatch::handle(&sh, &conn, req).await
    }));
    match (&mut handler.0).await {
        Ok(Ok(reply)) => ServerFrame::Ok { id, reply },
        Ok(Err(error)) => ServerFrame::Err { id, error },
        Err(join) => {
            tracing::error!(error = %join, "request handler failed");
            err_frame(id, ErrorCode::Internal, "internal error")
        }
    }
}

/// Aborts the wrapped task when dropped.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The single writer task of a connection: owns the write half and writes frames in
/// the order they were queued. Ends on a write error or timeout, which also ends the
/// request loop (its sender sees the receiver gone).
async fn write_loop(mut wr: OwnedWriteHalf, mut rx: mpsc::Receiver<Outgoing>) {
    while let Some((frame, _permit)) = rx.recv().await {
        match write_bounded(&mut wr, &frame).await {
            Ok(()) => {}
            // Only this reply is lost: answer its id with TooLarge and keep serving.
            Err(ProtoError::FrameTooLarge(len)) => match &frame {
                ServerFrame::Ok { id, .. } => {
                    tracing::warn!(len, id, "reply too large; answering TooLarge");
                    let fallback = err_frame(
                        *id,
                        ErrorCode::TooLarge,
                        "reply exceeds 8 MiB; lower the limit",
                    );
                    if write_bounded(&mut wr, &fallback).await.is_err() {
                        return;
                    }
                }
                _ => return,
            },
            Err(e) => {
                tracing::debug!(error = %e, "write failed; dropping replies");
                return;
            }
        }
    }
}

/// `write_json` under `WRITE_TIMEOUT`: a peer that stops reading ends the connection
/// instead of holding its in-flight permits forever.
async fn write_bounded(wr: &mut OwnedWriteHalf, frame: &ServerFrame) -> Result<(), ProtoError> {
    match tokio::time::timeout(WRITE_TIMEOUT, write_json(wr, frame)).await {
        Ok(result) => result,
        Err(_) => Err(ProtoError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "frame write timed out",
        ))),
    }
}

/// Removes the socket file, tolerating its absence.
fn remove_socket_file(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(error = %e, "could not remove the socket file"),
    }
}

/// Unauthenticated error frame (id 0) before the connection is closed. Bounded by
/// `REJECT_TIMEOUT`, and its failure is ignored: the connection is closed either way.
async fn reject(stream: &mut UnixStream, message: &str) {
    let frame = err_frame(0, ErrorCode::Unauthenticated, message);
    let _ = tokio::time::timeout(REJECT_TIMEOUT, write_json(stream, &frame)).await;
}

fn err_frame(id: u64, code: ErrorCode, message: impl Into<String>) -> ServerFrame {
    ServerFrame::Err {
        id,
        error: ErrorBody::new(code, message),
    }
}

/// `agora daemon`: build a multi-thread runtime, bind, run. Logging is initialised
/// by the caller (the CLI).
pub fn run_blocking(config: DaemonConfig) -> Result<(), DaemonError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async { Daemon::bind(config).await?.run().await })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_crypto::Identity;
    use agora_proto::{ErrorCode, Reply, write_frame};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Upper bound on every wait, so a bug fails the test instead of hanging it.
    const WAIT: Duration = Duration::from_secs(5);

    static NEXT_HOME: AtomicUsize = AtomicUsize::new(0);

    /// A short unique home under /tmp (macOS caps unix socket paths at 104 bytes),
    /// removed when dropped.
    struct TestHome(PathBuf);

    impl TestHome {
        fn new() -> Self {
            let n = NEXT_HOME.fetch_add(1, Ordering::Relaxed);
            let dir = PathBuf::from(format!("/tmp/agsrv-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }

        fn config(&self) -> DaemonConfig {
            DaemonConfig {
                home: self.0.clone(),
                handle_signals: false,
                ..DaemonConfig::default()
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn recv(stream: &mut UnixStream) -> ServerFrame {
        let bytes = tokio::time::timeout(WAIT, read_frame(stream, MAX_FRAME))
            .await
            .expect("server answered in time")
            .expect("frame readable")
            .expect("not EOF");
        serde_json::from_slice(&bytes).expect("server frame is JSON")
    }

    /// Connects and completes the handshake as `id`; returns the authenticated stream.
    async fn connect_as(socket: &Path, id: &Identity) -> UnixStream {
        let mut stream = tokio::time::timeout(WAIT, UnixStream::connect(socket))
            .await
            .expect("connect in time")
            .expect("connect to daemon");
        let challenge = match recv(&mut stream).await {
            ServerFrame::Hello(hello) => {
                agora_proto::unb64(&hello.challenge, "challenge").expect("base64 challenge")
            }
            other => panic!("expected hello, got {other:?}"),
        };
        write_json(&mut stream, &ClientHello::sign(id, &challenge, "test"))
            .await
            .expect("send client hello");
        match recv(&mut stream).await {
            ServerFrame::Welcome(welcome) => {
                assert_eq!(welcome.pubkey, hex(&id.public()));
            }
            other => panic!("expected welcome, got {other:?}"),
        }
        stream
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ping_is_answered_after_the_handshake_and_shutdown_removes_the_socket() {
        let home = TestHome::new();
        let daemon = Daemon::bind(home.config()).await.expect("bind");
        let socket = daemon.socket_path().to_path_buf();
        let token = daemon.shutdown_token();
        let run = tokio::spawn(daemon.run());

        let id = Identity::from_seed([9; 32]);
        let mut stream = connect_as(&socket, &id).await;
        write_json(
            &mut stream,
            &Request {
                id: 1,
                req: Req::Ping,
            },
        )
        .await
        .expect("send ping");
        assert_eq!(
            recv(&mut stream).await,
            ServerFrame::Ok {
                id: 1,
                reply: Reply::Pong
            }
        );

        token.cancel();
        let result = tokio::time::timeout(WAIT, run)
            .await
            .expect("run ends after cancellation")
            .expect("run task joins");
        assert!(result.is_ok(), "run failed: {result:?}");
        assert!(!socket.exists(), "socket file is removed on shutdown");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn second_bind_on_a_live_home_is_already_running_and_leaves_the_socket_alone() {
        let home = TestHome::new();
        let first = Daemon::bind(home.config()).await.expect("first bind");
        let socket = first.socket_path().to_path_buf();
        let token = first.shutdown_token();
        let run = tokio::spawn(first.run());

        let second = Daemon::bind(home.config())
            .await
            .err()
            .expect("second bind on a live home must fail");
        assert!(
            matches!(second, DaemonError::AlreadyRunning(_)),
            "got {second:?}"
        );
        assert!(socket.exists(), "the first daemon's socket is untouched");

        token.cancel();
        tokio::time::timeout(WAIT, run)
            .await
            .expect("run ends after cancellation")
            .expect("run task joins")
            .expect("first daemon stops cleanly");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn regular_file_at_the_socket_path_is_a_config_error() {
        let home = TestHome::new();
        std::fs::create_dir_all(&home.0).expect("create home");
        let planted = Paths::new(&home.0).socket;
        std::fs::write(&planted, b"not a socket").expect("plant a regular file");

        let err = Daemon::bind(home.config())
            .await
            .err()
            .expect("bind must refuse a non-socket at the socket path");
        assert!(matches!(err, DaemonError::Config(_)), "got {err:?}");
        assert!(planted.is_file(), "the planted file is not deleted");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn garbage_frame_is_bad_request_and_the_connection_keeps_serving() {
        let home = TestHome::new();
        let daemon = Daemon::bind(home.config()).await.expect("bind");
        let socket = daemon.socket_path().to_path_buf();
        let token = daemon.shutdown_token();
        let run = tokio::spawn(daemon.run());

        let id = Identity::from_seed([9; 32]);
        let mut stream = connect_as(&socket, &id).await;
        write_frame(&mut stream, b"{not json")
            .await
            .expect("send garbage frame");
        match recv(&mut stream).await {
            ServerFrame::Err { id: 0, error } => {
                assert_eq!(error.code, ErrorCode::BadRequest);
            }
            other => panic!("expected a BadRequest on id 0, got {other:?}"),
        }

        write_json(
            &mut stream,
            &Request {
                id: 2,
                req: Req::Ping,
            },
        )
        .await
        .expect("send ping");
        assert_eq!(
            recv(&mut stream).await,
            ServerFrame::Ok {
                id: 2,
                reply: Reply::Pong
            }
        );

        token.cancel();
        tokio::time::timeout(WAIT, run)
            .await
            .expect("run ends after cancellation")
            .expect("run task joins")
            .expect("daemon stops cleanly");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_that_never_reads_does_not_block_shutdown() {
        let home = TestHome::new();
        let daemon = Daemon::bind(home.config()).await.expect("bind");
        let socket = daemon.socket_path().to_path_buf();
        let token = daemon.shutdown_token();
        let run = tokio::spawn(daemon.run());

        let id = Identity::from_seed([9; 32]);
        let mut stream = connect_as(&socket, &id).await;
        write_json(
            &mut stream,
            &Request {
                id: 1,
                req: Req::BootstrapOwner,
            },
        )
        .await
        .expect("send bootstrap");
        assert_eq!(
            recv(&mut stream).await,
            ServerFrame::Ok {
                id: 1,
                reply: Reply::Unit
            }
        );
        let blob = vec![7u8; 1024 * 1024];
        write_json(
            &mut stream,
            &Request {
                id: 2,
                req: Req::PutBlob { bytes: b64(&blob) },
            },
        )
        .await
        .expect("send put");
        let hash = match recv(&mut stream).await {
            ServerFrame::Ok {
                id: 2,
                reply: Reply::BlobHash { hash },
            } => hash,
            other => panic!("expected BlobHash, got {other:?}"),
        };

        // Each reply is a 1 MiB blob, so the client's socket buffer fills after a
        // reply or two. The client pipelines 200 fetches and never reads any reply.
        let (_unread, mut writes) = stream.into_split();
        let mut flood = tokio::spawn(async move {
            for n in 0..200u64 {
                let req = Request {
                    id: 100 + n,
                    req: Req::GetBlob { hash: hash.clone() },
                };
                if write_json(&mut writes, &req).await.is_err() {
                    break;
                }
            }
            writes
        });
        // Lets the daemon reach its in-flight limit against the unread replies. The
        // assertion below holds wherever it has got to.
        let _ = tokio::time::timeout(Duration::from_millis(500), &mut flood).await;

        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(15), run)
            .await
            .expect("run ends within 15 s although the client never reads")
            .expect("run task joins");
        assert!(result.is_ok(), "run failed: {result:?}");
        flood.abort();
    }

    /// Polls `cond` until it holds; fails the test if it does not within `within`.
    async fn wait_until(within: Duration, cond: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + within;
        while !cond() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "condition not reached within {within:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn client_closing_with_a_full_window_stops_its_inbox_waits_promptly() {
        let home = TestHome::new();
        let mut config = home.config();
        config.max_inflight_per_conn = 2;
        let daemon = Daemon::bind(config).await.expect("bind");
        let socket = daemon.socket_path().to_path_buf();
        let token = daemon.shutdown_token();
        // Every running InboxWait holds one subscription on the bus.
        let bus = daemon.shared().bus.clone();
        let run = tokio::spawn(daemon.run());

        let id = Identity::from_seed([9; 32]);
        let mut stream = connect_as(&socket, &id).await;
        // An unregistered identity's wait fails at once, so register it first.
        write_json(
            &mut stream,
            &Request {
                id: 1,
                req: Req::BootstrapOwner,
            },
        )
        .await
        .expect("send bootstrap");
        assert_eq!(
            recv(&mut stream).await,
            ServerFrame::Ok {
                id: 1,
                reply: Reply::Unit
            }
        );
        // The window of 2 plus one: the third request waits for a permit.
        for n in 2..=4u64 {
            let req = Request {
                id: n,
                req: Req::InboxWait {
                    after: 0,
                    timeout_ms: 30_000,
                    limit: 10,
                },
            };
            write_json(&mut stream, &req)
                .await
                .expect("send inbox wait");
        }
        wait_until(WAIT, || bus.receiver_count() == 2).await;

        drop(stream);
        wait_until(Duration::from_secs(3), || bus.receiver_count() == 0).await;

        token.cancel();
        let result = tokio::time::timeout(WAIT, run)
            .await
            .expect("run ends after cancellation")
            .expect("run task joins");
        assert!(result.is_ok(), "run failed: {result:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_reply_is_too_large_under_its_id_and_the_connection_keeps_serving() {
        let (server_end, mut client) = UnixStream::pair().expect("socketpair");
        let (_reads, wr) = server_end.into_split();
        let (tx, rx) = mpsc::channel::<Outgoing>(4);
        let writer = tokio::spawn(write_loop(wr, rx));

        // Its frame would exceed MAX_FRAME, so nothing for it reaches the wire.
        let oversized = ServerFrame::Ok {
            id: 5,
            reply: Reply::Blob {
                bytes: "A".repeat(MAX_FRAME),
            },
        };
        tx.send((oversized, None))
            .await
            .expect("queue oversized reply");
        tx.send((
            ServerFrame::Ok {
                id: 6,
                reply: Reply::Pong,
            },
            None,
        ))
        .await
        .expect("queue pong");
        drop(tx);

        match recv(&mut client).await {
            ServerFrame::Err { id: 5, error } => assert_eq!(error.code, ErrorCode::TooLarge),
            other => panic!("expected TooLarge on id 5, got {other:?}"),
        }
        assert_eq!(
            recv(&mut client).await,
            ServerFrame::Ok {
                id: 6,
                reply: Reply::Pong
            }
        );
        tokio::time::timeout(WAIT, writer)
            .await
            .expect("writer ends once its queue is drained")
            .expect("writer task joins");
    }
}
