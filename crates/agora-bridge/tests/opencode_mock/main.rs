//! The opencode bridge against a hand-written fake of `opencode serve`: a TCP listener on
//! 127.0.0.1:0 that parses HTTP/1.1 requests and answers canned responses. `GET /event` stays open
//! as a chunked SSE stream that the test feeds frame by frame.
#![cfg(feature = "opencode")]

use agora_bridge::opencode::{OpencodeHttp, OpencodeOptions, auth_from_env, drive, run};
use agora_bridge::{BridgeError, Delivery};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const SESSION: &str = "ses_test";
/// Bound on anything the test waits for.
const T: Duration = Duration::from_secs(5);
/// How long a delivery must stay unanswered to count as blocked.
const QUIET: Duration = Duration::from_millis(300);
/// Lets the driver read the frames the fake has written before the test acts on them.
const SETTLE: Duration = Duration::from_millis(200);

/// One request as the fake received it.
#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    auth: Option<String>,
    body: Value,
}

/// State shared by the fake's connection tasks and the test.
struct Shared {
    requests: Mutex<Vec<Recorded>>,
    /// Raw bytes for the open `/event` stream; each send is one HTTP chunk.
    frames: broadcast::Sender<Vec<u8>>,
    /// Number of `/event` streams opened so far.
    streams: watch::Sender<usize>,
    /// Status answered to `prompt_async` (204 unless a test changes it).
    prompt_status: AtomicU16,
}

/// The fake server as the test sees it.
struct Fake {
    url: String,
    shared: Arc<Shared>,
    streams: watch::Receiver<usize>,
}

impl Fake {
    async fn start() -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let (streams_tx, streams) = watch::channel(0);
        let shared = Arc::new(Shared {
            requests: Mutex::new(Vec::new()),
            frames: broadcast::channel(64).0,
            streams: streams_tx,
            prompt_status: AtomicU16::new(204),
        });
        tokio::spawn(accept_loop(listener, Arc::clone(&shared)));
        Fake {
            url: format!("http://{addr}"),
            shared,
            streams,
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.shared.requests.lock().expect("log").clone()
    }

    fn prompts(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.path.ends_with("/prompt_async"))
            .collect()
    }

    fn set_prompt_status(&self, status: u16) {
        self.shared.prompt_status.store(status, Ordering::SeqCst);
    }

    /// Writes raw bytes to the open event stream, as one chunk.
    fn push(&self, bytes: &[u8]) {
        let _ = self.shared.frames.send(bytes.to_vec());
    }

    /// Waits until the driver has opened `GET /event`.
    async fn wait_for_stream(&mut self) {
        self.streams
            .wait_for(|n| *n > 0)
            .await
            .expect("fake is alive");
    }
}

async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(serve(stream, Arc::clone(&shared)));
    }
}

/// Serves requests on one connection until the client closes it or opens `/event`.
async fn serve(stream: TcpStream, shared: Arc<Shared>) {
    let mut reader = BufReader::new(stream);
    while let Some(req) = read_request(&mut reader).await {
        if req.method == "GET" && req.path == "/event" {
            // Recorded like the other requests, so a test can check the auth it carried.
            shared.requests.lock().expect("log").push(req);
            stream_events(reader.get_mut(), &shared).await;
            return;
        }
        let (status, body) = answer(&req, &shared);
        // Recorded before the answer goes out, so a test that saw the reply also sees the request.
        shared.requests.lock().expect("log").push(req);
        write_response(reader.get_mut(), status, &body).await;
    }
}

/// Reads one request: request line, headers, and a `Content-Length` body. `None` at end of stream.
async fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Recorded> {
    let mut line = String::new();
    if reader.read_line(&mut line).await.ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut length: usize = 0;
    let mut auth = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).await.ok()? == 0 {
            return None;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = value.parse().ok()?,
                "authorization" => auth = Some(value.to_owned()),
                _ => {}
            }
        }
    }
    let mut raw = vec![0; length];
    reader.read_exact(&mut raw).await.ok()?;
    let body = if raw.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&raw)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&raw).into_owned()))
    };
    Some(Recorded {
        method,
        path,
        auth,
        body,
    })
}

/// The canned answer to one request: (status, body).
fn answer(req: &Recorded, shared: &Shared) -> (u16, String) {
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/session") => (200, json!({"id": SESSION, "title": "agora"}).to_string()),
        ("GET", path) if path == format!("/session/{SESSION}") => {
            (200, json!({"id": SESSION}).to_string())
        }
        ("POST", path) if path == format!("/session/{SESSION}/prompt_async") => {
            let status = shared.prompt_status.load(Ordering::SeqCst);
            (
                status,
                if status < 300 {
                    String::new()
                } else {
                    "boom".into()
                },
            )
        }
        _ => (404, "{}".into()),
    }
}

async fn write_response(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        404 => "Not Found",
        _ => "Error",
    };
    let head = if status == 204 {
        format!("HTTP/1.1 {status} {reason}\r\n\r\n")
    } else {
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    let _ = stream.write_all(head.as_bytes()).await;
}

/// Holds the `/event` connection open as a chunked SSE stream fed by `Shared::frames`.
async fn stream_events(stream: &mut TcpStream, shared: &Shared) {
    // Subscribe before counting the stream, so a frame the test sends after `wait_for_stream` is
    // never missed.
    let mut frames = shared.frames.subscribe();
    shared.streams.send_modify(|n| *n += 1);
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\n\r\n";
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    loop {
        match frames.recv().await {
            Ok(frame) => {
                let mut chunk = format!("{:x}\r\n", frame.len()).into_bytes();
                chunk.extend_from_slice(&frame);
                chunk.extend_from_slice(b"\r\n");
                if stream.write_all(&chunk).await.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// A running `drive` and the handles the test uses to feed and stop it.
struct Driver {
    deliveries: mpsc::Sender<Delivery>,
    shutdown: CancellationToken,
    task: JoinHandle<Result<(), BridgeError>>,
}

/// A busy bound no ordinary test reaches.
const LONG_BUSY: Duration = Duration::from_secs(60);
/// The busy bound of the test that checks the bound itself.
const SHORT_BUSY: Duration = Duration::from_secs(1);

fn spawn_drive(client: OpencodeHttp) -> Driver {
    spawn_drive_bounded(client, LONG_BUSY)
}

fn spawn_drive_bounded(client: OpencodeHttp, busy_max_wait: Duration) -> Driver {
    let (deliveries, rx) = mpsc::channel(4);
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(drive(
        client,
        SESSION.to_owned(),
        rx,
        shutdown.clone(),
        busy_max_wait,
    ));
    Driver {
        deliveries,
        shutdown,
        task,
    }
}

impl Driver {
    /// Sends one prompt; returns the receiver of its answer.
    async fn deliver(&self, text: &str) -> oneshot::Receiver<bool> {
        let (reply, answer) = oneshot::channel();
        let delivery = Delivery {
            text: text.to_owned(),
            max_seq: 1,
            count: 1,
            reply,
        };
        self.deliveries
            .send(delivery)
            .await
            .expect("driver is running");
        answer
    }

    /// Stops the driver and checks that it ends cleanly.
    async fn finish(self) {
        self.shutdown.cancel();
        timeout(T, self.task)
            .await
            .expect("driver stops on shutdown")
            .expect("driver task did not panic")
            .expect("driver returns Ok");
    }
}

/// The answer to a delivery, which must arrive within `T`.
async fn answered(answer: oneshot::Receiver<bool>) -> bool {
    timeout(T, answer)
        .await
        .expect("answered in time")
        .expect("reply sent")
}

fn sse(event: &Value) -> String {
    format!("data: {event}\n\n")
}

fn status_frame(session: &str, status: &str) -> String {
    sse(&json!({
        "type": "session.status",
        "properties": {"sessionID": session, "status": {"type": status}},
    }))
}

fn idle_frame(session: &str) -> String {
    sse(&json!({"type": "session.idle", "properties": {"sessionID": session}}))
}

mod auth_url;
mod delivery;
mod session;
