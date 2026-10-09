//! Socket client: connect (auto-spawning the daemon), handshake, request/response.

use crate::{ClientError, Ctx};
use agora_crypto::Identity;
use agora_daemon::Paths;
use agora_proto::{
    ClientHello, Event, MAX_WAIT_MS, Reply, Req, Request, ServerFrame, read_json, unb64, write_json,
};
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio::net::UnixStream;

pub struct Client {
    stream: UnixStream,
    /// Id of the next request; the first request is 1 (0 is reserved for connection errors).
    next: u64,
    /// Set while a call is in flight and left set unless that call ends cleanly (an
    /// answer or a `Remote` refusal). A timeout, I/O, framing or EOF error, or a future
    /// dropped mid-exchange, leaves the stream unmatched to requests, so later calls are
    /// refused.
    broken: bool,
}

/// How long an auto-spawned daemon may take to start listening.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Interval between connection probes while the daemon starts.
const SPAWN_POLL: Duration = Duration::from_millis(25);
/// How long the handshake may take before the daemon counts as unresponsive.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an ordinary request may take before the daemon counts as unresponsive.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Slack added to an InboxWait's own wait, for the reply to travel back.
const WAIT_SLACK: Duration = Duration::from_secs(5);

impl Client {
    /// Connect to ctx.paths.socket. If the socket is missing or refuses (NotFound /
    /// ConnectionRefused) and env AGORA_NO_SPAWN is not "1", call `ensure_daemon`
    /// (in spawn_blocking) and connect again. Then handshake as ctx.identity with
    /// ctx.label.
    pub async fn connect(ctx: &Ctx) -> Result<Client, ClientError> {
        let socket = &ctx.paths.socket;
        let stream = match UnixStream::connect(socket).await {
            Ok(stream) => stream,
            Err(e) if is_absent(&e) && !no_spawn_requested() => {
                let paths = ctx.paths.clone();
                tokio::task::spawn_blocking(move || ensure_daemon(&paths))
                    .await
                    .map_err(|e| ClientError::Daemon(format!("spawn task failed: {e}")))??;
                UnixStream::connect(socket).await?
            }
            Err(e) if is_absent(&e) => return Err(ClientError::Daemon("not running".into())),
            Err(e) => return Err(e.into()),
        };
        within(
            HANDSHAKE_TIMEOUT,
            Self::handshake(stream, &ctx.identity, &ctx.label),
        )
        .await
    }

    /// Connect and handshake without ever spawning. Missing/refused socket ->
    /// ClientError::Daemon("not running").
    pub async fn connect_no_spawn(
        paths: &Paths,
        id: &Identity,
        label: &str,
    ) -> Result<Client, ClientError> {
        let stream = UnixStream::connect(&paths.socket).await.map_err(|e| {
            if is_absent(&e) {
                ClientError::Daemon("not running".into())
            } else {
                ClientError::Io(e)
            }
        })?;
        within(HANDSHAKE_TIMEOUT, Self::handshake(stream, id, label)).await
    }

    /// Send `Request{id: next, req}` and read frames until the `Ok`/`Err` with that
    /// id (Event frames are skipped). Err -> ClientError::Remote(body). EOF ->
    /// ClientError::Daemon("connection closed"). No answer within the call's deadline ->
    /// ClientError::Daemon("daemon did not answer within N s").
    pub async fn call(&mut self, req: Req) -> Result<Reply, ClientError> {
        if self.broken {
            return Err(unusable());
        }
        let limit = call_limit(&req);
        let id = self.next;
        self.next = self.next.saturating_add(1);
        // Set before the exchange: if this future is dropped mid-exchange, the flag stays set.
        self.broken = true;
        let result = within(limit, self.exchange(id, req)).await;
        self.settle(&result);
        result
    }

    /// Clear the broken flag once an exchange has returned. A `Remote` error is the
    /// daemon's well-formed answer to this very request, so the stream stays usable.
    fn settle<T>(&mut self, result: &Result<T, ClientError>) {
        self.broken = match result {
            Ok(_) | Err(ClientError::Remote(_)) => false,
            Err(_) => true,
        };
    }

    /// Write request `id` and read frames until its answer arrives.
    async fn exchange(&mut self, id: u64, req: Req) -> Result<Reply, ClientError> {
        write_json(&mut self.stream, &Request { id, req }).await?;
        loop {
            match read_json::<ServerFrame, _>(&mut self.stream).await? {
                Some(ServerFrame::Ok { id: got, reply }) if got == id => return Ok(reply),
                Some(ServerFrame::Err { id: got, error }) if got == id => {
                    return Err(ClientError::Remote(error));
                }
                // Events and answers to other ids are not ours to act on.
                Some(_) => continue,
                None => return Err(ClientError::Daemon("connection closed".into())),
            }
        }
    }

    /// Hello -> signed ClientHello -> Welcome. A server `Err` at either step is the
    /// reason for refusal and is surfaced as `Remote`.
    async fn handshake(
        mut stream: UnixStream,
        id: &Identity,
        label: &str,
    ) -> Result<Client, ClientError> {
        let hello = match read_json::<ServerFrame, _>(&mut stream).await? {
            Some(ServerFrame::Hello(hello)) => hello,
            Some(ServerFrame::Err { error, .. }) => return Err(ClientError::Remote(error)),
            Some(_) => return Err(ClientError::UnexpectedReply),
            None => return Err(ClientError::Daemon("connection closed".into())),
        };
        let challenge = unb64(&hello.challenge, "challenge")?;
        write_json(&mut stream, &ClientHello::sign(id, &challenge, label)).await?;
        match read_json::<ServerFrame, _>(&mut stream).await? {
            Some(ServerFrame::Welcome(_)) => Ok(Client {
                stream,
                next: 1,
                broken: false,
            }),
            Some(ServerFrame::Err { error, .. }) => Err(ClientError::Remote(error)),
            Some(_) => Err(ClientError::UnexpectedReply),
            None => Err(ClientError::Daemon("connection closed".into())),
        }
    }
}

impl Client {
    /// Turn this connection into an event stream: send `Req::Subscribe{channels}`
    /// (empty = every channel the identity is a member of, now and later) and wait for
    /// `Reply::Subscribed`, keeping any `Event` frames that arrive before it so none is
    /// lost. The call deadline applies to the subscribe request itself. A refusal
    /// (`NotMember`, inactive identity, ...) is `ClientError::Remote`.
    ///
    /// A failed subscribe consumes the client, so the broken flag needs no update here.
    pub async fn subscribe(mut self, channels: Vec<String>) -> Result<EventStream, ClientError> {
        if self.broken {
            return Err(unusable());
        }
        let id = self.next;
        self.next = self.next.saturating_add(1);
        let mut early = VecDeque::new();
        let subscribed = within(
            CALL_TIMEOUT,
            self.exchange_subscribe(id, channels, &mut early),
        )
        .await?;
        Ok(EventStream {
            client: self,
            channels: subscribed,
            early,
        })
    }

    /// Write the Subscribe request `id` and read frames until its answer arrives.
    /// `Event` frames that precede the answer are pushed onto `early`.
    async fn exchange_subscribe(
        &mut self,
        id: u64,
        channels: Vec<String>,
        early: &mut VecDeque<Event>,
    ) -> Result<Vec<String>, ClientError> {
        let req = Req::Subscribe { channels };
        write_json(&mut self.stream, &Request { id, req }).await?;
        loop {
            match read_json::<ServerFrame, _>(&mut self.stream).await? {
                Some(ServerFrame::Ok { id: got, reply }) if got == id => {
                    return match reply {
                        Reply::Subscribed { channels } => Ok(channels),
                        _ => Err(ClientError::UnexpectedReply),
                    };
                }
                Some(ServerFrame::Err { id: got, error }) if got == id => {
                    return Err(ClientError::Remote(error));
                }
                Some(ServerFrame::Event { event }) => early.push_back(event),
                // Answers to other ids are not ours to act on.
                Some(_) => continue,
                None => return Err(ClientError::Daemon("connection closed".into())),
            }
        }
    }
}

/// A subscribed connection: the daemon pushes `Event`s without being asked. Events are
/// notifications, not data: after `Event::Lagged` the caller re-reads its inbox (the
/// inbox cursor is the source of truth).
pub struct EventStream {
    client: Client,
    /// The effective channel set the daemon reported in `Subscribed`.
    channels: Vec<String>,
    /// Events that arrived between the Subscribe request and its reply.
    early: VecDeque<Event>,
}

impl EventStream {
    /// The channels the daemon reported in `Reply::Subscribed`.
    pub fn channels(&self) -> &[String] {
        &self.channels
    }

    /// The next event: buffered early events first, then frames read from the socket
    /// (non-Event frames are skipped). `Ok(None)` = the daemon closed the connection.
    /// There is no timeout: the caller selects on it, e.g. in `tokio::select!`.
    ///
    /// Not cancel-safe: reading a frame is not, so if this future is dropped while a
    /// frame is partly read, the stream is out of step with the socket. Drop the stream
    /// too in that case; do not call `next` again on it.
    pub async fn next(&mut self) -> Result<Option<Event>, ClientError> {
        if let Some(event) = self.early.pop_front() {
            return Ok(Some(event));
        }
        loop {
            match read_json::<ServerFrame, _>(&mut self.client.stream).await? {
                Some(ServerFrame::Event { event }) => return Ok(Some(event)),
                Some(_) => continue,
                None => return Ok(None),
            }
        }
    }
}

/// How long the daemon may take to answer `req`. A wait is held by the daemon for up to its own
/// `timeout_ms` (clamped to `MAX_WAIT_MS`), so its limit is that wait plus `WAIT_SLACK`; any other
/// request gets `CALL_TIMEOUT`.
fn call_limit(req: &Req) -> Duration {
    match req {
        Req::InboxWait { .. } => Duration::from_millis(MAX_WAIT_MS) + WAIT_SLACK,
        Req::WorkWait { timeout_ms, .. } => {
            Duration::from_millis((*timeout_ms).min(MAX_WAIT_MS)) + WAIT_SLACK
        }
        _ => CALL_TIMEOUT,
    }
}

/// Run `fut` for at most `limit`; running out of time means the daemon did not answer.
async fn within<T>(
    limit: Duration,
    fut: impl std::future::Future<Output = Result<T, ClientError>>,
) -> Result<T, ClientError> {
    match tokio::time::timeout(limit, fut).await {
        Ok(result) => result,
        Err(_) => Err(ClientError::Daemon(format!(
            "daemon did not answer within {} s",
            limit.as_secs()
        ))),
    }
}

/// Refusal of every call on a connection an earlier error left out of step.
fn unusable() -> ClientError {
    ClientError::Daemon("connection unusable after an earlier error; reconnect".into())
}

/// The socket has no listener behind it: nothing is bound there, or the bound file
/// is stale and refuses connections.
fn is_absent(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused)
}

fn no_spawn_requested() -> bool {
    matches!(std::env::var("AGORA_NO_SPAWN").as_deref(), Ok("1"))
}

/// A blocking probe: is anything accepting connections on `socket`?
fn socket_live(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

/// Make sure a daemon serves `paths.socket`. Under an exclusive blocking flock on
/// `paths.spawn_lock` (std File::lock; home created 0700 first): if a connect now
/// succeeds, return; else spawn `std::env::current_exe()` with args
/// `["daemon", "--home", <home>]`, stdin/stdout null, stderr appended to
/// `paths.log`, `process_group(0)` so it outlives us; then try to connect every
/// 25 ms for up to 5 s (startup only). Timeout -> ClientError::Daemon("did not start; see <log>");
/// if the spawned process exited before the deadline, the error names its exit status:
/// "daemon exited (<status>); see <log>".
pub fn ensure_daemon(paths: &Paths) -> Result<(), ClientError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&paths.home)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&paths.spawn_lock)?;
    lock.lock()?;

    // Everything below runs under the lock; the lock is released when `lock` drops.
    if socket_live(&paths.socket) {
        return Ok(());
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .truncate(false)
        .mode(0o600)
        .open(&paths.log)?;
    // The daemon outlives this command, so the handle is only watched until the deadline.
    let mut child = Command::new(std::env::current_exe()?)
        .arg("daemon")
        .arg("--home")
        .arg(&paths.home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .process_group(0)
        .spawn()?;

    let outcome = await_socket(paths, &mut child);
    // Reap the child on every path, in the background: an unwaited child stays a zombie
    // for as long as this process lives, and waiting here would block on the daemon.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    outcome
}

/// Poll the socket until it is live or the deadline passes. A daemon that exits early
/// may still lose to another one that holds the lock and becomes live in time, so the
/// socket is polled to the deadline either way.
fn await_socket(paths: &Paths, child: &mut Child) -> Result<(), ClientError> {
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    let mut exited = None;
    while !socket_live(&paths.socket) {
        if Instant::now() >= deadline {
            return Err(match exited {
                Some(status) => ClientError::Daemon(format!(
                    "daemon exited ({status}); see {}",
                    paths.log.display()
                )),
                None => ClientError::Daemon(format!("did not start; see {}", paths.log.display())),
            });
        }
        if exited.is_none() {
            exited = child.try_wait()?;
        }
        std::thread::sleep(SPAWN_POLL);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_proto::{ErrorBody, ErrorCode, Event, PROTO_VERSION, ServerHello, Welcome, b64};
    use tokio::net::UnixListener;
    use tokio::time::timeout;

    #[test]
    fn a_work_wait_may_take_as_long_as_its_own_timeout() {
        let wait = |timeout_ms| Req::WorkWait {
            spec: agora_sched::model::WaitSpec::Tasks {
                ids: vec!["job-1".into()],
                any: false,
            },
            timeout_ms,
            cursor: None,
        };
        // The daemon holds a wait for its timeout; a 30 s call limit would give up on a healthy daemon.
        assert_eq!(call_limit(&wait(55_000)), Duration::from_secs(60));
        assert_eq!(call_limit(&wait(u64::MAX)), Duration::from_secs(60));
        assert_eq!(call_limit(&wait(1_000)), Duration::from_secs(6));
        assert_eq!(call_limit(&Req::Whoami), CALL_TIMEOUT);
    }

    #[tokio::test]
    async fn connect_no_spawn_to_missing_socket_is_not_running() {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::new(dir.path());
        let id = Identity::from_seed([7u8; 32]);
        match Client::connect_no_spawn(&paths, &id, "test").await {
            Err(ClientError::Daemon(m)) => assert_eq!(m, "not running"),
            Err(other) => panic!("expected Daemon(not running), got {other:?}"),
            Ok(_) => panic!("connected to a socket that does not exist"),
        }
    }

    #[tokio::test]
    async fn server_error_before_hello_surfaces_as_remote() {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::new(dir.path());
        let listener = UnixListener::bind(&paths.socket).expect("bind fake daemon");
        let server = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.expect("accept");
            let frame = ServerFrame::Err {
                id: 0,
                error: ErrorBody::new(ErrorCode::Unauthenticated, "peer uid mismatch"),
            };
            write_json(&mut conn, &frame).await.expect("send err");
        });

        let id = Identity::from_seed([7u8; 32]);
        match Client::connect_no_spawn(&paths, &id, "test").await {
            Err(ClientError::Remote(body)) => assert_eq!(body.code, ErrorCode::Unauthenticated),
            Err(other) => panic!("expected Remote, got {other:?}"),
            Ok(_) => panic!("handshake succeeded against a refusing server"),
        }
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn handshake_then_call_skips_events_and_returns_matching_reply() {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::new(dir.path());
        let listener = UnixListener::bind(&paths.socket).expect("bind fake daemon");
        let challenge = [9u8; 32];
        let expected_key = Identity::from_seed([7u8; 32]).public();
        let server = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.expect("accept");
            let hello = ServerHello {
                version: PROTO_VERSION,
                challenge: b64(&challenge),
                daemon: "agora/test".into(),
            };
            write_json(&mut conn, &ServerFrame::Hello(hello))
                .await
                .expect("send hello");
            let client_hello: ClientHello = read_json(&mut conn)
                .await
                .expect("read client hello")
                .expect("client hello frame");
            let proven = client_hello.verify(&challenge).expect("signature verifies");
            assert_eq!(proven, expected_key);
            let welcome = Welcome {
                pubkey: client_hello.pubkey.clone(),
                pseudonym: "p".into(),
            };
            write_json(&mut conn, &ServerFrame::Welcome(welcome))
                .await
                .expect("send welcome");
            let request: Request = read_json(&mut conn)
                .await
                .expect("read request")
                .expect("request frame");
            assert_eq!(
                request,
                Request {
                    id: 1,
                    req: Req::Ping
                }
            );
            let event = Event::ChannelChanged {
                channel: "c".into(),
            };
            write_json(&mut conn, &ServerFrame::Event { event })
                .await
                .expect("send event");
            write_json(
                &mut conn,
                &ServerFrame::Ok {
                    id: 1,
                    reply: Reply::Pong,
                },
            )
            .await
            .expect("send reply");
        });

        let id = Identity::from_seed([7u8; 32]);
        let mut client = Client::connect_no_spawn(&paths, &id, "test")
            .await
            .expect("handshake");
        let reply = client.call(Req::Ping).await.expect("ping");
        assert_eq!(reply, Reply::Pong);
        server.await.expect("server task");
    }

    /// Every wait in these tests is bounded, so a bug fails instead of hanging.
    const WAIT: Duration = Duration::from_secs(5);

    /// Fake daemon side: accept one connection and complete the handshake.
    async fn accept_welcomed(listener: &UnixListener) -> UnixStream {
        let challenge = [9u8; 32];
        let (mut conn, _) = listener.accept().await.expect("accept");
        let hello = ServerHello {
            version: PROTO_VERSION,
            challenge: b64(&challenge),
            daemon: "agora/test".into(),
        };
        write_json(&mut conn, &ServerFrame::Hello(hello))
            .await
            .expect("send hello");
        let client_hello: ClientHello = read_json(&mut conn)
            .await
            .expect("read client hello")
            .expect("client hello frame");
        client_hello.verify(&challenge).expect("signature verifies");
        let welcome = Welcome {
            pubkey: client_hello.pubkey.clone(),
            pseudonym: "p".into(),
        };
        write_json(&mut conn, &ServerFrame::Welcome(welcome))
            .await
            .expect("send welcome");
        conn
    }

    /// Fake daemon side: read the first request and check it is `Subscribe{channels}` with id 1.
    async fn expect_subscribe(conn: &mut UnixStream, channels: &[&str]) {
        let request: Request = timeout(WAIT, read_json::<Request, _>(&mut *conn))
            .await
            .expect("request arrives in time")
            .expect("read request")
            .expect("request frame");
        let req = Req::Subscribe {
            channels: channels.iter().map(|c| c.to_string()).collect(),
        };
        assert_eq!(request, Request { id: 1, req });
    }

    async fn connect_test_client(paths: &Paths) -> Client {
        let id = Identity::from_seed([7u8; 32]);
        timeout(WAIT, Client::connect_no_spawn(paths, &id, "test"))
            .await
            .expect("handshake in time")
            .expect("handshake")
    }

    #[tokio::test]
    async fn subscribe_then_next_yields_event_sent_after_reply() {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::new(dir.path());
        let listener = UnixListener::bind(&paths.socket).expect("bind fake daemon");
        let server = tokio::spawn(async move {
            let mut conn = accept_welcomed(&listener).await;
            expect_subscribe(&mut conn, &["c"]).await;
            let reply = Reply::Subscribed {
                channels: vec!["c".into()],
            };
            write_json(&mut conn, &ServerFrame::Ok { id: 1, reply })
                .await
                .expect("send reply");
            let event = Event::ChannelChanged {
                channel: "c".into(),
            };
            write_json(&mut conn, &ServerFrame::Event { event })
                .await
                .expect("send event");
        });

        let client = connect_test_client(&paths).await;
        let mut stream = timeout(WAIT, client.subscribe(vec!["c".into()]))
            .await
            .expect("subscribe in time")
            .expect("subscribe");
        assert_eq!(stream.channels(), &["c".to_string()][..]);
        let event = timeout(WAIT, stream.next())
            .await
            .expect("event in time")
            .expect("next");
        assert_eq!(
            event,
            Some(Event::ChannelChanged {
                channel: "c".into()
            })
        );
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn event_sent_before_subscribed_reply_is_returned_first() {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::new(dir.path());
        let listener = UnixListener::bind(&paths.socket).expect("bind fake daemon");
        let server = tokio::spawn(async move {
            let mut conn = accept_welcomed(&listener).await;
            expect_subscribe(&mut conn, &[]).await;
            let early = Event::ChannelChanged {
                channel: "early".into(),
            };
            write_json(&mut conn, &ServerFrame::Event { event: early })
                .await
                .expect("send early event");
            let reply = Reply::Subscribed {
                channels: vec!["early".into()],
            };
            write_json(&mut conn, &ServerFrame::Ok { id: 1, reply })
                .await
                .expect("send reply");
        });

        let client = connect_test_client(&paths).await;
        let mut stream = timeout(WAIT, client.subscribe(vec![]))
            .await
            .expect("subscribe in time")
            .expect("subscribe");
        let event = timeout(WAIT, stream.next())
            .await
            .expect("event in time")
            .expect("next");
        assert_eq!(
            event,
            Some(Event::ChannelChanged {
                channel: "early".into()
            })
        );
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn subscribe_refusal_surfaces_as_remote_with_code() {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::new(dir.path());
        let listener = UnixListener::bind(&paths.socket).expect("bind fake daemon");
        let server = tokio::spawn(async move {
            let mut conn = accept_welcomed(&listener).await;
            expect_subscribe(&mut conn, &["c"]).await;
            let error = ErrorBody::new(ErrorCode::NotMember, "c");
            write_json(&mut conn, &ServerFrame::Err { id: 1, error })
                .await
                .expect("send err");
        });

        let client = connect_test_client(&paths).await;
        match timeout(WAIT, client.subscribe(vec!["c".into()]))
            .await
            .expect("subscribe in time")
        {
            Err(ClientError::Remote(body)) => assert_eq!(body.code, ErrorCode::NotMember),
            Err(other) => panic!("expected Remote, got {other:?}"),
            Ok(_) => panic!("subscribe succeeded against a refusing server"),
        }
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn next_returns_none_when_server_closes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::new(dir.path());
        let listener = UnixListener::bind(&paths.socket).expect("bind fake daemon");
        let server = tokio::spawn(async move {
            let mut conn = accept_welcomed(&listener).await;
            expect_subscribe(&mut conn, &["c"]).await;
            let reply = Reply::Subscribed {
                channels: vec!["c".into()],
            };
            write_json(&mut conn, &ServerFrame::Ok { id: 1, reply })
                .await
                .expect("send reply");
            // Dropping the connection closes it.
        });

        let client = connect_test_client(&paths).await;
        let mut stream = timeout(WAIT, client.subscribe(vec!["c".into()]))
            .await
            .expect("subscribe in time")
            .expect("subscribe");
        let next = timeout(WAIT, stream.next())
            .await
            .expect("EOF in time")
            .expect("next");
        assert_eq!(next, None);
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn call_after_a_mid_frame_close_is_refused_as_unusable() {
        use tokio::io::AsyncWriteExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::new(dir.path());
        let listener = UnixListener::bind(&paths.socket).expect("bind fake daemon");
        let server = tokio::spawn(async move {
            let mut conn = accept_welcomed(&listener).await;
            let request: Request = timeout(WAIT, read_json::<Request, _>(&mut conn))
                .await
                .expect("request arrives in time")
                .expect("read request")
                .expect("request frame");
            assert_eq!(
                request,
                Request {
                    id: 1,
                    req: Req::Ping
                }
            );
            // A header promising 64 bytes, then 3 of them, then the connection closes.
            conn.write_all(&64u32.to_be_bytes())
                .await
                .expect("send header");
            conn.write_all(b"{\"i").await.expect("send partial body");
        });

        let mut client = connect_test_client(&paths).await;
        let first = timeout(WAIT, client.call(Req::Ping))
            .await
            .expect("first call in time");
        assert!(
            matches!(first, Err(ClientError::Io(_) | ClientError::Proto(_))),
            "a truncated frame is an I/O or proto error, got {first:?}"
        );
        server.await.expect("server task");

        match timeout(WAIT, client.call(Req::Ping))
            .await
            .expect("second call in time")
        {
            Err(ClientError::Daemon(m)) => {
                assert_eq!(m, "connection unusable after an earlier error; reconnect")
            }
            other => panic!("expected the unusable refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_call_dropped_mid_exchange_leaves_the_client_unusable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let paths = Paths::new(dir.path());
        let listener = UnixListener::bind(&paths.socket).expect("bind fake daemon");
        let server = tokio::spawn(async move {
            let mut conn = accept_welcomed(&listener).await;
            let _ = read_json::<Request, _>(&mut conn).await;
            // Read the request, then never answer.
            std::future::pending::<()>().await;
        });

        let mut client = connect_test_client(&paths).await;
        // The caller gives up after 50 ms and drops the call future mid-exchange.
        let dropped = timeout(Duration::from_millis(50), client.call(Req::Ping)).await;
        assert!(dropped.is_err(), "the call must still be waiting");
        match timeout(WAIT, client.call(Req::Ping))
            .await
            .expect("second call in time")
        {
            Err(ClientError::Daemon(m)) => {
                assert_eq!(m, "connection unusable after an earlier error; reconnect")
            }
            other => panic!("expected the unusable refusal, got {other:?}"),
        }
        server.abort();
    }
}
