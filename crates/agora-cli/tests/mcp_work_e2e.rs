//! End-to-end tests of the work queue over `agora mcp` (stdio), against the real binary. These are the only tests
//! that run a job through the whole stack: the daemon spawns `agora _supervise` for it, so the command really runs.
//! Each test gets its own AGORA_HOME and folders, so each folder is a distinct identity: `alice` (the owner, author
//! of the queued task), `bob` (the claimant that runs it) and `carol` (a worker that is neither). `Env::drop` stops
//! the daemon, and each `Proc` guard kills its child. Every read from a child's pipe carries a deadline.

use assert_cmd::assert::Assert;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

/// Hard cap on one CLI invocation: a hang fails the test instead of stalling the run.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for one protocol line. Every job in these tests ends within a few seconds.
const WAIT: Duration = Duration::from_secs(20);
const LEGACY: &str = "2025-11-25";

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
        for who in ["alice", "bob", "carol"] {
            fs::create_dir(folders.path().join(who)).expect("user folder");
        }
        Self { home, folders }
    }

    fn home_arg(&self) -> &str {
        self.home.path().to_str().expect("UTF-8 home path")
    }

    /// The folder of `who` as a canonical path. The daemon compares a task's working directory with its realpath,
    /// and on macOS the temp dir is a symlink.
    fn folder(&self, who: &str) -> String {
        let path = self.folders.path().join(who);
        fs::canonicalize(path)
            .expect("canonical folder")
            .to_str()
            .expect("UTF-8 folder path")
            .to_owned()
    }

    /// An `agora` process: the shared home holds the daemon, `who`'s folder is the identity.
    fn process(&self, who: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agora"));
        cmd.env("AGORA_HOME", self.home.path())
            .env("AGORA_LOG", "warn")
            .env_remove("AGORA_FOLDER")
            .env_remove("AGORA_NO_SPAWN")
            .env_remove("WORK_SCOPE")
            .current_dir(self.folders.path().join(who));
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

    /// alice bootstraps the owner, then turns off the machine gates in this home. The idle, memory and load readings
    /// are the host's, so with the defaults a job could be held before its process exists. The owner sets them; the
    /// `--home` flag is the one the operator would use. Then bob and carol register as workers.
    fn bootstrap(&self) {
        self.json("alice", &["init", "--owner"]);
        self.agora(
            "alice",
            &[
                "--home",
                self.home_arg(),
                "work",
                "settings",
                "--idle-min",
                "0",
                "--memory-limit",
                "off",
                "--memory-budget",
                "off",
                "--load-factor",
                "1000000",
            ],
        )
        .success();
        self.json("bob", &["init"]);
        self.json("carol", &["init"]);
    }

    /// `agora mcp` over stdio, in the folder of `who`.
    fn stdio(&self, who: &str) -> Proc {
        let mut cmd = self.process(who);
        cmd.arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        Proc::new(cmd.spawn().expect("spawn agora mcp"))
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Best effort: an env that never started a daemon has nothing to stop.
        let _ = self.process("alice").arg("stop").output();
    }
}

/// An `agora mcp` child on stdio. Its stdout arrives on a channel, so every read carries a deadline. Dropping it
/// kills the process if it is still running.
struct Proc {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: i64,
}

impl Proc {
    fn new(mut child: Child) -> Self {
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            next_id: 0,
        }
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").expect("write request");
        self.stdin.flush().expect("flush request");
    }

    /// Sends a request and returns its response.
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self
                .lines
                .recv_timeout(WAIT)
                .unwrap_or_else(|e| panic!("no output within {WAIT:?}: {e:?}"));
            let msg: Value = serde_json::from_str(&line).expect("stdout line is JSON");
            if msg.get("id") == Some(&json!(id)) && msg.get("method").is_none() {
                return msg;
            }
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An `agora mcp` session in `who`'s folder, past the legacy handshake.
fn session(env: &Env, who: &str) -> Proc {
    let mut server = env.stdio(who);
    let reply = server.call(
        "initialize",
        json!({
            "protocolVersion": LEGACY,
            "capabilities": {},
            "clientInfo": {"name": "e2e", "version": "1.0"},
        }),
    );
    assert_eq!(reply["result"]["protocolVersion"], LEGACY, "{reply}");
    server.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    server
}

/// Calls a tool and returns its whole result (`content` and `structuredContent`); a tool error fails the test.
fn call_tool(server: &mut Proc, name: &str, arguments: Value) -> Value {
    let reply = server.call("tools/call", json!({"name": name, "arguments": arguments}));
    assert_eq!(reply["result"]["isError"], false, "{name}: {reply}");
    reply["result"].clone()
}

/// Calls a tool and returns its structured content.
fn tool(server: &mut Proc, name: &str, arguments: Value) -> Value {
    call_tool(server, name, arguments)["structuredContent"].clone()
}

/// alice queues `t1`, a command that prints one line, in her folder. bob claims it, reads its digest with work_show
/// and accepts it with that digest. Bob's session then holds the claimed, accepted task.
fn queued_and_accepted(env: &Env, alice: &mut Proc, bob: &mut Proc) {
    let added = tool(
        alice,
        "work_add",
        json!({
            "id": "t1",
            "command": ["sh", "-c", "echo hello-from-job"],
            "cwd": env.folder("alice"),
        }),
    );
    assert_eq!(added["task"]["id"], "t1", "{added}");

    let next = tool(bob, "work_next", json!({}));
    assert_eq!(next["claimed"]["id"], "t1", "{next}");

    let shown = tool(bob, "work_show", json!({"id": "t1"}));
    let digest = shown["digest"].as_str().expect("show carries a digest");
    let accepted = tool(bob, "work_accept", json!({"id": "t1", "digest": digest}));
    assert_eq!(accepted["task"]["id"], "t1", "{accepted}");
}

/// bob runs the accepted `t1` and waits for it to end: the structured `{task, wait}` of work_run.
fn run_accepted(bob: &mut Proc) -> Value {
    tool(bob, "work_run", json!({"task": "t1", "timeout_secs": 30}))
}

#[test]
fn claimant_run_of_an_accepted_task_exits_with_status_zero() {
    let env = Env::new();
    env.bootstrap();
    let mut alice = session(&env, "alice");
    let mut bob = session(&env, "bob");
    queued_and_accepted(&env, &mut alice, &mut bob);

    let out = run_accepted(&mut bob);

    assert_eq!(out["wait"]["status"], 0, "{out}");
    assert_eq!(out["wait"]["timed_out"], false, "{out}");
}

#[test]
fn claimant_run_carries_the_job_output_in_the_wait_tail() {
    let env = Env::new();
    env.bootstrap();
    let mut alice = session(&env, "alice");
    let mut bob = session(&env, "bob");
    queued_and_accepted(&env, &mut alice, &mut bob);

    let out = run_accepted(&mut bob);

    assert_eq!(out["wait"]["tail"], json!(["hello-from-job"]), "{out}");
}

#[test]
fn author_waiting_on_the_task_sees_the_job_output_in_the_tail() {
    let env = Env::new();
    env.bootstrap();
    let mut alice = session(&env, "alice");
    let mut bob = session(&env, "bob");
    queued_and_accepted(&env, &mut alice, &mut bob);
    run_accepted(&mut bob);

    let waited = tool(
        &mut alice,
        "work_wait",
        json!({"tasks": ["t1"], "timeout_secs": 30}),
    );

    assert_eq!(waited["status"], 0, "{waited}");
    assert_eq!(waited["tail"], json!(["hello-from-job"]), "{waited}");
}

#[test]
fn worker_that_is_neither_author_nor_claimant_gets_an_empty_tail() {
    let env = Env::new();
    env.bootstrap();
    let mut alice = session(&env, "alice");
    let mut bob = session(&env, "bob");
    queued_and_accepted(&env, &mut alice, &mut bob);
    run_accepted(&mut bob);
    let mut carol = session(&env, "carol");

    let waited = tool(
        &mut carol,
        "work_wait",
        json!({"tasks": ["t1"], "timeout_secs": 30}),
    );

    assert_eq!(waited["status"], 0, "the job ended with status 0: {waited}");
    assert_eq!(waited["tail"], json!([]), "{waited}");
}

#[test]
fn inline_command_reports_its_own_exit_status() {
    let env = Env::new();
    env.bootstrap();
    let mut bob = session(&env, "bob");

    let out = tool(
        &mut bob,
        "work_run",
        json!({
            "command": ["sh", "-c", "exit 3"],
            "cwd": env.folder("bob"),
            "timeout_secs": 30,
        }),
    );

    assert_eq!(out["wait"]["status"], 3, "{out}");
    assert_eq!(out["wait"]["timed_out"], false, "{out}");
}

#[test]
fn timed_out_run_leaves_the_job_running_and_work_wait_resumes_with_the_cursor_to_status_zero() {
    let env = Env::new();
    env.bootstrap();
    let mut bob = session(&env, "bob");
    let out = tool(
        &mut bob,
        "work_run",
        json!({
            "command": ["sleep", "5"],
            "cwd": env.folder("bob"),
            "timeout_secs": 1,
        }),
    );
    assert_eq!(out["wait"]["timed_out"], true, "{out}");
    let task = out["task"]["id"]
        .as_str()
        .expect("the started task has an id");
    let cursor = out["wait"]["cursor"]
        .as_str()
        .expect("a timed-out wait returns a cursor");

    let resumed = tool(
        &mut bob,
        "work_wait",
        json!({"tasks": [task], "cursor": cursor, "timeout_secs": 30}),
    );

    assert_eq!(resumed["status"], 0, "{resumed}");
    assert_eq!(resumed["timed_out"], false, "{resumed}");
}

#[test]
fn claimant_run_text_frames_the_job_output_as_untrusted_data() {
    let env = Env::new();
    env.bootstrap();
    let mut alice = session(&env, "alice");
    let mut bob = session(&env, "bob");
    queued_and_accepted(&env, &mut alice, &mut bob);

    let result = call_tool(
        &mut bob,
        "work_run",
        json!({"task": "t1", "timeout_secs": 30}),
    );

    let text = result["content"][0]["text"].as_str().expect("a text block");
    let open = text.find("<untrusted-data>").expect("the text is framed");
    let close = text.find("</untrusted-data>").expect("the frame closes");
    let line = text
        .find("hello-from-job")
        .expect("the text shows the job output");
    assert!(
        open < line && line < close,
        "job output outside the frame: {text}"
    );
}
