#![allow(dead_code)]
//! Shared harness for daemon integration tests: `mod common; use common::*;`.
//! Every test using it must be `#[tokio::test(flavor = "multi_thread", worker_threads = 4)]`.

use agora_crypto::Identity;
use agora_daemon::clock::Clock;
use agora_daemon::{Daemon, DaemonConfig, DaemonError, Injected, now_ms};
use agora_proto::{
    ClientHello, ErrorBody, MessageWire, OpWire, Reply, Req, Request, ServerFrame, read_json,
    unb64, write_json,
};
use agora_run::machine::Machine;
use agora_store::{SignedMessage, SignedOp};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Upper bound on every wait in the harness, so a daemon bug fails instead of hanging.
const WAIT: Duration = Duration::from_secs(30);

/// An in-process daemon on a fresh temp home under /tmp (short path: macOS limits
/// socket paths to 104 bytes): `tempfile::Builder::new().prefix("ag").tempdir_in("/tmp")`.
pub struct TestDaemon {
    pub home: tempfile::TempDir,
    pub socket: PathBuf,
    pub token: CancellationToken,
    pub task: JoinHandle<Result<(), DaemonError>>,
}

/// Defaults (DaemonConfig::default()) except: home = temp dir, handle_signals = false.
pub async fn start() -> TestDaemon {
    start_with(|_| {}).await
}

/// Like `start`, with `f` applied to the config before `Daemon::bind`.
pub async fn start_with(f: impl FnOnce(&mut DaemonConfig)) -> TestDaemon {
    start_injected(None, None, f).await
}

/// Like `start_with`, with an injected clock and/or machine probe (`None` = the real one). Every start
/// helper points `supervisor_argv` at the `agora-supervise` test binary.
pub async fn start_injected(
    clock: Option<Arc<dyn Clock>>,
    machine: Option<Arc<dyn Machine>>,
    f: impl FnOnce(&mut DaemonConfig),
) -> TestDaemon {
    let home = tempfile::Builder::new()
        .prefix("ag")
        .tempdir_in("/tmp")
        .expect("create temp home under /tmp");
    let (socket, token, task) = bind_and_spawn(home.path(), Injected { clock, machine }, f)
        .await
        .expect("bind daemon on a fresh home");
    TestDaemon {
        home,
        socket,
        token,
        task,
    }
}

/// Bind a daemon on an EXISTING home (for stale-socket / second-daemon tests).
pub async fn start_at(
    home: &Path,
    f: impl FnOnce(&mut DaemonConfig),
) -> Result<(CancellationToken, JoinHandle<Result<(), DaemonError>>), DaemonError> {
    let (_socket, token, task) = bind_and_spawn(home, Injected::default(), f).await?;
    Ok((token, task))
}

/// Load the config for `home`, disable signal handling, apply `f`, bind, and spawn
/// the run task. A bind failure returns before anything is spawned.
async fn bind_and_spawn(
    home: &Path,
    injected: Injected,
    f: impl FnOnce(&mut DaemonConfig),
) -> Result<
    (
        PathBuf,
        CancellationToken,
        JoinHandle<Result<(), DaemonError>>,
    ),
    DaemonError,
> {
    let mut cfg = DaemonConfig::load(home)?;
    cfg.handle_signals = false;
    cfg.supervisor_argv = Some(vec![env!("CARGO_BIN_EXE_agora-supervise").into()]);
    f(&mut cfg);
    let daemon = Daemon::bind_with(cfg, injected).await?;
    let socket = daemon.socket_path().to_path_buf();
    let token = daemon.shutdown_token();
    let task = tokio::spawn(daemon.run());
    Ok((socket, token, task))
}

impl TestDaemon {
    /// Cancel the token and await the run task (must return Ok within 5 s).
    pub async fn stop(self) {
        self.token.cancel();
        match tokio::time::timeout(Duration::from_secs(5), self.task).await {
            Err(_) => panic!("daemon did not stop within 5 s of cancellation"),
            Ok(Err(join)) => panic!("daemon run task panicked: {join}"),
            Ok(Ok(Err(e))) => panic!("daemon run returned an error: {e}"),
            Ok(Ok(Ok(()))) => {}
        }
    }
}

/// Deterministic identity: Identity::from_seed([seed; 32]).
pub fn ident(seed: u8) -> Identity {
    Identity::from_seed([seed; 32])
}

/// A connection that completed the handshake.
pub struct TestClient {
    pub id: Identity,
    pub stream: UnixStream,
    pub next: u64,
}

/// Read one server frame, failing the test on EOF, a read error, or silence.
async fn next_frame(stream: &mut UnixStream, what: &str) -> ServerFrame {
    let read = tokio::time::timeout(WAIT, read_json::<ServerFrame, _>(stream)).await;
    match read {
        Err(_) => panic!("no {what} frame within {WAIT:?}"),
        Ok(Err(e)) => panic!("reading {what} frame failed: {e}"),
        Ok(Ok(None)) => panic!("daemon closed the connection before the {what} frame"),
        Ok(Ok(Some(frame))) => frame,
    }
}

/// Connect and handshake as `id` (label "test"); panics on any failure.
pub async fn connect(d: &TestDaemon, id: Identity) -> TestClient {
    let mut stream = raw_connect(d).await;
    let challenge = match next_frame(&mut stream, "Hello").await {
        ServerFrame::Hello(hello) => {
            unb64(&hello.challenge, "challenge").expect("Hello challenge is base64")
        }
        other => panic!("expected Hello, got {other:?}"),
    };
    let hello = ClientHello::sign(&id, &challenge, "test");
    write_json(&mut stream, &hello)
        .await
        .expect("send ClientHello");
    match next_frame(&mut stream, "Welcome").await {
        ServerFrame::Welcome(_) => {}
        other => panic!("expected Welcome after ClientHello, got {other:?}"),
    }
    TestClient {
        id,
        stream,
        next: 1,
    }
}

/// Connected socket with NO handshake done (raw protocol tests).
pub async fn raw_connect(d: &TestDaemon) -> UnixStream {
    UnixStream::connect(&d.socket)
        .await
        .expect("connect to daemon socket")
}

/// Process-unique counter for nonces and message ids.
fn next_counter(counter: &AtomicU64) -> u64 {
    counter.fetch_add(1, Ordering::Relaxed)
}

static NONCE: AtomicU64 = AtomicU64::new(0);
static MESSAGE: AtomicU64 = AtomicU64::new(0);

impl TestClient {
    /// Send Request{id: next, req}; read frames until the Ok/Err with that id.
    pub async fn call(&mut self, req: Req) -> Result<Reply, ErrorBody> {
        let id = self.next;
        self.next += 1;
        write_json(&mut self.stream, &Request { id, req })
            .await
            .expect("send request");
        loop {
            match next_frame(&mut self.stream, &format!("reply to request {id}")).await {
                ServerFrame::Ok { id: got, reply } if got == id => return Ok(reply),
                ServerFrame::Err { id: got, error } if got == id => return Err(error),
                _ => {}
            }
        }
    }

    /// `call` that panics with the error body on Err.
    pub async fn ok(&mut self, req: Req) -> Reply {
        match self.call(req).await {
            Ok(reply) => reply,
            Err(error) => panic!("request failed: {error}"),
        }
    }

    /// OpWire signed now (real clock) with a fresh unique nonce.
    pub fn op(&self, kind: &str, subject: &str, arg: &str) -> OpWire {
        let nonce = format!("n-{}-{}", std::process::id(), next_counter(&NONCE));
        let signed = SignedOp::new(kind, subject, arg, now_ms(), &nonce).sign(&self.id);
        OpWire::from_op(&signed)
    }

    /// hex64 of this client's public key.
    pub fn pk_hex(&self) -> String {
        agora_store::hex(&self.id.public())
    }

    /// Post a plaintext message (fresh unique id) and return its seq; panics on error.
    pub async fn post(&mut self, channel: &str, text: &str) -> i64 {
        let msg = SignedMessage {
            id: format!("m-{}", next_counter(&MESSAGE)),
            channel: channel.to_string(),
            thread_of: None,
            sender: [0; 32],
            ts: now_ms(),
            encrypted: false,
            epoch: 0,
            body: text.as_bytes().to_vec(),
            attachments: vec![],
            sig: vec![],
        }
        .sign(&self.id);
        let req = Req::Post {
            msg: MessageWire::from_message(&msg),
        };
        match self.ok(req).await {
            Reply::Posted { seq } => seq,
            other => panic!("post: expected Posted, got {other:?}"),
        }
    }

    /// Create a plaintext "group" channel with these members (role "member"); panics on error.
    pub async fn create_group(&mut self, channel: &str, members: &[&TestClient]) {
        let listed: Vec<serde_json::Value> = members
            .iter()
            .map(|m| serde_json::json!({"pk": m.pk_hex(), "role": "member"}))
            .collect();
        let arg = serde_json::json!({"kind": "group", "members": listed}).to_string();
        let op = self.op("channel.create", channel, &arg);
        match self.ok(Req::ChannelCreate { op, keys: vec![] }).await {
            Reply::Unit => {}
            other => panic!("create_group: expected Unit, got {other:?}"),
        }
    }
}

/// Clients for a fresh world: owner (seed 1, BootstrapOwner), alice (3), bob (4),
/// carol (7) — each Register'ed with its name as label.
pub struct World {
    pub owner: TestClient,
    pub alice: TestClient,
    pub bob: TestClient,
    pub carol: TestClient,
}

pub async fn world(d: &TestDaemon) -> World {
    let mut owner = connect(d, ident(1)).await;
    match owner.ok(Req::BootstrapOwner).await {
        Reply::Unit => {}
        other => panic!("BootstrapOwner: expected Unit, got {other:?}"),
    }
    let alice = register(d, 3, "alice").await;
    let bob = register(d, 4, "bob").await;
    let carol = register(d, 7, "carol").await;
    World {
        owner,
        alice,
        bob,
        carol,
    }
}

/// Connect as `ident(seed)` and register it under `label`; panics on failure.
async fn register(d: &TestDaemon, seed: u8, label: &str) -> TestClient {
    let mut client = connect(d, ident(seed)).await;
    match client
        .ok(Req::Register {
            label: label.to_string(),
        })
        .await
    {
        Reply::Unit => {}
        other => panic!("Register {label}: expected Unit, got {other:?}"),
    }
    client
}
