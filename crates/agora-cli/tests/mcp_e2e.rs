//! End-to-end tests of `agora mcp` (stdio and HTTP) and `agora mcp token`, against the real
//! binary. Every test gets its own AGORA_HOME and folders, so each folder is a distinct identity.
//! `Env::drop` stops the daemon the test started, and each child process is killed by its `Proc`
//! guard. Every read from a child's pipe carries a deadline, so a hang fails instead of stalling.

use assert_cmd::assert::Assert;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Hard cap on one CLI invocation: a hang fails the test instead of stalling the run.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for one protocol line, one HTTP reply, or a daemon's start-up line.
const WAIT: Duration = Duration::from_secs(20);
/// Closing stdin must end `agora mcp` within this.
const EXIT_WAIT: Duration = Duration::from_secs(5);

const LEGACY: &str = "2025-11-25";
const MODERN: &str = "2026-07-28";
const TOOL_COUNT: usize = 60;
const PING: &str = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

struct Env {
    home: tempfile::TempDir,
    folders: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        // Short prefix under /tmp: the daemon socket path must fit in 104 bytes.
        let home = tempfile::Builder::new()
            .prefix("ag")
            .tempdir_in("/tmp")
            .expect("home dir");
        let folders = tempfile::tempdir().expect("folders dir");
        for who in ["alice", "bob"] {
            fs::create_dir(folders.path().join(who)).expect("user folder");
        }
        Self { home, folders }
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn folder(&self, who: &str) -> PathBuf {
        self.folders.path().join(who)
    }

    /// An `agora` process: the shared home holds the daemon, `who`'s folder is the identity.
    fn process(&self, who: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agora"));
        cmd.env("AGORA_HOME", self.home())
            .env("AGORA_LOG", "warn")
            .env_remove("AGORA_FOLDER")
            .env_remove("AGORA_NO_SPAWN")
            .current_dir(self.folder(who));
        cmd
    }

    fn agora(&self, who: &str, args: &[&str]) -> Assert {
        assert_cmd::Command::from_std(self.process(who))
            .args(args)
            .timeout(CMD_TIMEOUT)
            .assert()
    }

    /// Run with `--json` and require success: stdout is one JSON document.
    fn json(&self, who: &str, args: &[&str]) -> Value {
        let mut full = args.to_vec();
        full.push("--json");
        let assert = self.agora(who, &full).success();
        serde_json::from_slice(&assert.get_output().stdout).expect("stdout is one JSON document")
    }

    /// `agora mcp` over stdio, in the folder of `who`. The daemon is started first: the server's
    /// daemon link starts with its first request and would otherwise auto-spawn the daemon in
    /// the background, racing `agora stop` when the test ends.
    fn stdio(&self, who: &str) -> Proc {
        self.json(who, &["whoami"]);
        let mut cmd = self.process(who);
        cmd.arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        Proc::new(cmd.spawn().expect("spawn agora mcp"))
    }

    /// `agora daemon --mcp-http ADDR`: the daemon with MCP over HTTP in one process. Returns once
    /// the front-end is listening.
    fn daemon_with_http(&self, addr: SocketAddr) -> Proc {
        let mut cmd = self.process("alice");
        cmd.args(["daemon", "--mcp-http", &addr.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let proc = Proc::new(cmd.spawn().expect("spawn agora daemon"));
        proc.wait_for_line(&format!("http://{addr}/mcp"));
        proc
    }

    /// `agora mcp --http ADDR` next to a daemon that is already running. Returns once listening.
    fn standalone_http(&self, addr: SocketAddr) -> Proc {
        let mut cmd = self.process("alice");
        cmd.args(["mcp", "--http", &addr.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let proc = Proc::new(cmd.spawn().expect("spawn agora mcp --http"));
        proc.wait_for_line(&format!("http://{addr}/mcp"));
        proc
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Best effort: an env that never started a daemon has nothing to stop.
        let _ = self.process("alice").arg("stop").output();
    }
}

/// A child process whose piped output arrives on a channel, so every read carries a deadline.
/// The output pipe is stdout for `agora mcp` over stdio, stderr for the HTTP servers.
/// Dropping it kills the process if it is still running.
struct Proc {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    /// Every stdout line the stdio session has read, in order.
    transcript: Vec<String>,
    next_id: i64,
}

impl Proc {
    fn new(mut child: Child) -> Self {
        let stdin = child.stdin.take();
        let source: Box<dyn Read + Send> = match (child.stdout.take(), child.stderr.take()) {
            (Some(out), _) => Box::new(out),
            (None, Some(err)) => Box::new(err),
            (None, None) => panic!("the child has no output pipe"),
        };
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(source).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child: Some(child),
            stdin,
            lines,
            transcript: Vec::new(),
            next_id: 0,
        }
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        writeln!(stdin, "{msg}").expect("write request");
        stdin.flush().expect("flush request");
    }

    /// Sends a request and returns its response. Other lines read on the way are recorded.
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self
                .next_line(WAIT)
                .expect("stdout closed before the response");
            let msg: Value = serde_json::from_str(&line).expect("stdout line is JSON");
            self.transcript.push(line);
            if msg.get("id") == Some(&json!(id)) && msg.get("method").is_none() {
                return msg;
            }
        }
    }

    fn notify(&mut self, method: &str) {
        self.send(&json!({"jsonrpc": "2.0", "method": method}));
    }

    /// Closes stdin and requires the process to end within `EXIT_WAIT`. Output read on the way
    /// is recorded. Returns the exit status.
    fn close_stdin(&mut self) -> ExitStatus {
        self.stdin.take();
        self.exit_within()
    }

    /// Requires the process to end within `EXIT_WAIT` (its output pipe reaches EOF), then reaps
    /// it and returns its exit status.
    fn exit_within(&mut self) -> ExitStatus {
        let deadline = Instant::now() + EXIT_WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => self.transcript.push(line),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    panic!("the process did not end within {EXIT_WAIT:?}")
                }
            }
        }
        let mut child = self.child.take().expect("the process is not reaped yet");
        child.wait().expect("wait for the process")
    }

    /// The next output line; panics past `limit`.
    fn next_line(&self, limit: Duration) -> Option<String> {
        match self.lines.recv_timeout(limit) {
            Ok(line) => Some(line),
            Err(RecvTimeoutError::Disconnected) => None,
            Err(RecvTimeoutError::Timeout) => panic!("no output within {limit:?}"),
        }
    }

    /// Waits for the output line that contains `needle`; the process ending first is a failure.
    fn wait_for_line(&self, needle: &str) -> String {
        loop {
            let line = self
                .next_line(WAIT)
                .unwrap_or_else(|| panic!("the process ended before printing {needle}"));
            if line.contains(needle) {
                return line;
            }
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// One HTTP/1.1 reply: status, headers as sent, and the body text.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("reply body is JSON")
    }
}

/// POSTs `text` to /mcp over a new connection (`Connection: close`) and reads the reply to EOF.
fn post_mcp(addr: SocketAddr, headers: &[(&str, &str)], text: &str) -> Reply {
    let mut stream = TcpStream::connect_timeout(&addr, WAIT).expect("connect");
    stream.set_read_timeout(Some(WAIT)).expect("read timeout");
    stream.set_write_timeout(Some(WAIT)).expect("write timeout");
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
         Connection: close\r\n",
        text.len()
    );
    for (name, value) in headers {
        write!(request, "{name}: {value}\r\n").expect("format header");
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("send request head");
    stream
        .write_all(text.as_bytes())
        .expect("send request body");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read reply to EOF");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("reply has a header block");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = String::from_utf8_lossy(&raw[split + 4..]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("status code");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Reply {
        status,
        headers,
        body,
    }
}

/// A loopback address whose port was free a moment ago. The port is released before use, so
/// another process could take it in between; the tests accept that small race.
fn free_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    listener.local_addr().expect("local address")
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn pubkey(env: &Env, who: &str) -> String {
    env.json(who, &["whoami"])["pubkey"]
        .as_str()
        .expect("pubkey string")
        .to_owned()
}

/// alice bootstraps the owner, bob registers. Returns bob's public key.
fn owner_and_member(env: &Env) -> String {
    env.json("alice", &["init", "--owner"]);
    env.json("bob", &["init"])["pubkey"]
        .as_str()
        .expect("pubkey string")
        .to_owned()
}

/// The legacy handshake: `initialize`, then `notifications/initialized`.
fn initialize(server: &mut Proc) -> Value {
    let reply = server.call(
        "initialize",
        json!({
            "protocolVersion": LEGACY,
            "capabilities": {},
            "clientInfo": {"name": "e2e", "version": "1.0"},
        }),
    );
    server.notify("notifications/initialized");
    reply
}

/// The `_meta` block of a modern (2026-07-28, stateless) request.
fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": MODERN,
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

/// A token minted by `agora mcp token add`.
struct Minted {
    id: String,
    token: String,
}

fn add_token(env: &Env, label: &str) -> Minted {
    let added = env.json("alice", &["mcp", "token", "add", "--label", label]);
    Minted {
        id: added["id"].as_str().expect("id string").to_owned(),
        token: added["token"].as_str().expect("token string").to_owned(),
    }
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

#[test]
fn stdio_initialize_answers_the_legacy_version_it_was_asked_for() {
    let env = Env::new();
    let mut server = env.stdio("alice");

    let reply = initialize(&mut server);

    assert_eq!(reply["result"]["protocolVersion"], LEGACY, "{reply}");
}

#[test]
fn stdio_initialize_echoes_an_older_supported_legacy_version() {
    let env = Env::new();
    let mut server = env.stdio("alice");

    let reply = server.call(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1.0"}}),
    );

    assert_eq!(reply["result"]["protocolVersion"], "2025-06-18", "{reply}");
}

#[test]
fn stdio_initialize_counter_offers_2025_11_25_for_a_version_it_does_not_speak() {
    let env = Env::new();
    let mut server = env.stdio("alice");

    // 2026-07-28 is a modern version: it is not spoken on the legacy handshake.
    let reply = server.call(
        "initialize",
        json!({"protocolVersion": MODERN, "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1.0"}}),
    );

    assert_eq!(reply["result"]["protocolVersion"], LEGACY, "{reply}");
}

#[test]
fn stdio_tools_list_offers_all_60_tools() {
    let env = Env::new();
    let mut server = env.stdio("alice");
    initialize(&mut server);

    let reply = server.call("tools/list", json!({}));

    let tools = reply["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), TOOL_COUNT, "{reply}");
}

#[test]
fn stdio_whoami_signs_locally_with_this_folders_identity() {
    let env = Env::new();
    let mut server = env.stdio("alice");
    initialize(&mut server);

    let reply = server.call("tools/call", json!({"name": "whoami", "arguments": {}}));

    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let who = &reply["result"]["structuredContent"];
    assert_eq!(who["signing"], "local");
    let key = who["pubkey"].as_str().expect("pubkey string");
    assert!(is_hex64(key), "64 hex digits: {key}");
    assert_eq!(
        key,
        pubkey(&env, "alice"),
        "the CLI shows the same folder key"
    );
}

#[test]
fn stdio_stdout_carries_only_json_rpc_responses() {
    let env = Env::new();
    let mut server = env.stdio("alice");
    initialize(&mut server);
    server.call("tools/list", json!({}));
    server.call("tools/call", json!({"name": "whoami", "arguments": {}}));

    server.close_stdin();

    // initialize, tools/list and tools/call: three responses, and nothing else on stdout.
    assert_eq!(server.transcript.len(), 3, "{:?}", server.transcript);
    for line in &server.transcript {
        let msg: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line}"));
        assert_eq!(msg["jsonrpc"], "2.0", "{line}");
    }
}

#[test]
fn stdio_closing_stdin_ends_the_process_with_success_within_five_seconds() {
    let env = Env::new();
    let mut server = env.stdio("alice");
    initialize(&mut server);

    let status = server.close_stdin();

    assert!(status.success(), "exit status: {status}");
}

#[test]
fn server_discover_with_modern_meta_answers_complete_with_the_supported_versions() {
    let env = Env::new();
    let mut server = env.stdio("alice");

    let reply = server.call("server/discover", json!({"_meta": modern_meta()}));

    assert_eq!(reply["result"]["resultType"], "complete", "{reply}");
    assert_eq!(
        reply["result"]["supportedVersions"],
        json!(["2026-07-28", "2025-11-25"]),
        "{reply}"
    );
}

#[test]
fn an_unknown_method_is_refused_with_method_not_found() {
    let env = Env::new();
    let mut server = env.stdio("alice");

    let reply = server.call("resources/list", json!({"_meta": modern_meta()}));

    assert_eq!(reply["error"]["code"], -32601, "{reply}");
    assert!(reply.get("result").is_none(), "{reply}");
}

#[test]
fn a_chat_from_one_folders_mcp_server_is_in_another_folders_inbox_under_its_key() {
    let env = Env::new();
    let bob = owner_and_member(&env);
    env.json("alice", &["channel", "create", "general", "--member", &bob]);
    let mut server = env.stdio("bob");
    initialize(&mut server);

    let reply = server.call(
        "tools/call",
        json!({
            "name": "chat",
            "arguments": {"channel": "general", "text": "hello from bob's mcp"},
        }),
    );

    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let inbox = env.json("alice", &["inbox"]);
    let messages = inbox["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1, "exactly the one posted message: {inbox}");
    assert_eq!(messages[0]["text"], "hello from bob's mcp");
    assert_eq!(messages[0]["sender"], bob);
    assert_eq!(messages[0]["verified"], true);
}

#[test]
fn token_add_json_prints_id_label_token_pubkey_and_pseudonym() {
    let env = Env::new();

    let added = env.json("alice", &["mcp", "token", "add", "--label", "laptop"]);

    let keys: BTreeSet<&str> = added
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from(["id", "label", "token", "pubkey", "pseudonym"]),
        "{added}"
    );
    assert_eq!(added["label"], "laptop");
    assert!(
        added["token"]
            .as_str()
            .is_some_and(|t| t.starts_with("agora_")),
        "{added}"
    );
    assert!(is_hex64(added["pubkey"].as_str().expect("pubkey string")));
    assert!(added["pseudonym"].as_str().is_some_and(|p| !p.is_empty()));
}

#[test]
fn token_list_json_names_the_token_without_its_secrets() {
    let env = Env::new();
    let minted = add_token(&env, "laptop");

    let listed = env.json("alice", &["mcp", "token", "list"]);

    let rows = listed["tokens"].as_array().expect("tokens array");
    assert_eq!(rows.len(), 1, "{listed}");
    assert_eq!(rows[0]["id"], minted.id.as_str());
    assert_eq!(rows[0]["label"], "laptop");
    // The listing shows no secret: neither the token, nor the stored seed, nor its hash.
    let file: Value = serde_json::from_str(
        &fs::read_to_string(env.home().join("mcp-tokens.json")).expect("token file"),
    )
    .expect("token file is JSON");
    let stored = &file["tokens"][0];
    let shown = listed.to_string();
    assert!(!shown.contains(&minted.token), "token shown: {shown}");
    assert!(
        !shown.contains(stored["seed"].as_str().expect("seed")),
        "seed shown: {shown}"
    );
    assert!(
        !shown.contains(stored["token_sha256"].as_str().expect("hash")),
        "hash shown: {shown}"
    );
}

#[test]
fn token_file_is_readable_and_writable_only_by_its_owner() {
    let env = Env::new();
    add_token(&env, "laptop");

    let mode = fs::metadata(env.home().join("mcp-tokens.json"))
        .expect("token file")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(mode, 0o600);
}

#[test]
fn token_revoke_reports_revoked_and_the_token_leaves_the_list() {
    let env = Env::new();
    let minted = add_token(&env, "laptop");

    let revoked = env.json("alice", &["mcp", "token", "revoke", &minted.id]);

    assert_eq!(revoked["revoked"], true, "{revoked}");
    assert_eq!(
        env.json("alice", &["mcp", "token", "list"])["tokens"],
        json!([])
    );
}

#[test]
fn token_revoke_of_an_unknown_id_reports_not_revoked() {
    let env = Env::new();

    let revoked = env.json("alice", &["mcp", "token", "revoke", "t_00000000"]);

    assert_eq!(revoked["revoked"], false, "{revoked}");
}

#[test]
fn daemon_mcp_http_refuses_a_request_without_a_bearer_token_with_401_and_a_challenge() {
    let env = Env::new();
    // A token exists, so the refusal is about the missing header and not an empty token file.
    add_token(&env, "laptop");
    let addr = free_addr();
    let _daemon = env.daemon_with_http(addr);

    let reply = post_mcp(addr, &[], PING);

    assert_eq!(reply.status, 401);
    assert_eq!(
        reply.header("WWW-Authenticate"),
        Some("Bearer realm=\"agora\"")
    );
    assert_eq!(reply.json(), json!({"error": "unauthorized"}));
}

#[test]
fn daemon_mcp_http_answers_a_legacy_ping_with_a_token_from_token_add() {
    let env = Env::new();
    let minted = add_token(&env, "laptop");
    let addr = free_addr();
    let _daemon = env.daemon_with_http(addr);
    let authorization = bearer(&minted.token);

    let reply = post_mcp(addr, &[("Authorization", authorization.as_str())], PING);

    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        reply.json(),
        json!({"jsonrpc": "2.0", "id": 1, "result": {}})
    );
}

#[test]
fn agora_stop_ends_a_daemon_that_serves_mcp_http() {
    let env = Env::new();
    let addr = free_addr();
    let mut daemon = env.daemon_with_http(addr);

    assert_eq!(env.json("alice", &["stop"]), json!({"stopped": true}));

    let status = daemon.exit_within();
    assert!(status.success(), "exit status: {status}");
}

#[test]
fn standalone_mcp_http_answers_a_legacy_ping_with_a_token_from_token_add() {
    let env = Env::new();
    // The front-end talks to the daemon, so one must be running.
    env.json("alice", &["whoami"]);
    let minted = add_token(&env, "laptop");
    let addr = free_addr();
    let _mcp = env.standalone_http(addr);
    let authorization = bearer(&minted.token);

    let reply = post_mcp(addr, &[("Authorization", authorization.as_str())], PING);

    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(
        reply.json(),
        json!({"jsonrpc": "2.0", "id": 1, "result": {}})
    );
}

#[test]
fn standalone_mcp_http_refuses_a_non_loopback_address_without_allow_remote() {
    let env = Env::new();
    let addr = format!("0.0.0.0:{}", free_addr().port());

    let assert = env.agora("alice", &["mcp", "--http", &addr]).failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.starts_with("agora: mcp: "),
        "error on stderr: {stderr}"
    );
}

#[test]
fn mcp_http_combined_with_mcp_token_is_refused_and_writes_no_token_file() {
    let env = Env::new();

    let assert = env
        .agora(
            "alice",
            &[
                "mcp",
                "--http",
                "127.0.0.1:0",
                "token",
                "add",
                "--label",
                "x",
            ],
        )
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains("--http"), "error on stderr: {stderr}");
    assert!(
        !env.home().join("mcp-tokens.json").exists(),
        "no token file after a refused command"
    );
}
