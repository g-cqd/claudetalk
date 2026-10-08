//! End-to-end tests of `agora work …` against the real binary.
//!
//! Given-tasks mode (`work order|plan --tasks FILE`) is answered by the CLI with no daemon, so its stdout, stderr and
//! exit code are checked byte-for-byte against the differential fixtures of `agora-sched`. Every other verb runs
//! against a daemon that the first command spawns inside this test's own AGORA_HOME; `Env::drop` stops it, as
//! `cli.rs` does.
//!
//! Identities are folders: `alice` (the owner, `init --owner`) and `bob` (a worker, `init`) share one home.

use agora_daemon::Paths;
use assert_cmd::Command;
use assert_cmd::assert::Assert;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

/// Hard cap on one CLI invocation: a hang fails the test instead of stalling the run. Generous on purpose: the
/// suite runs on loaded hosts, and a slow daemon start must not look like a hang.
const CMD_TIMEOUT: Duration = Duration::from_secs(90);

/// How long a job may take to reach a state the test waits for, and the pause between reads of its state.
const STATE_WAIT: Duration = Duration::from_secs(60);
const STATE_POLL: Duration = Duration::from_millis(50);

/// Ordering fixtures that carry a `tasks` document: four `order`, three `plan`, one `assign`, and two refusals
/// (exit 2) of the given-tasks input.
const GIVEN_TASKS_CASES: [&str; 10] = [
    "order_critical_path",
    "order_text_full",
    "order_unicode_ids",
    "order_inherit_validity",
    "plan_two_agents_basic",
    "plan_text_three_agents",
    "plan_prefix_boundaries",
    "assign_basic_three_agents",
    "err_cycle_three",
    "err_duplicate_id",
];

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

    fn paths(&self) -> Paths {
        Paths::new(self.home.path())
    }

    fn folder(&self, who: &str) -> PathBuf {
        self.folders.path().join(who)
    }

    /// An `agora` invocation run as `who`: that user's folder is the identity, the shared home holds the daemon.
    /// The work defaults the caller's environment could supply (`WORK_SCOPE`) are cleared, so a test never depends
    /// on the shell that runs it.
    fn command(&self, who: &str) -> Command {
        let mut cmd = Command::cargo_bin("agora").expect("agora binary");
        cmd.env("AGORA_HOME", self.home.path())
            .env("AGORA_LOG", "warn")
            .env_remove("AGORA_FOLDER")
            .env_remove("AGORA_NO_SPAWN")
            .env_remove("WORK_SCOPE")
            .current_dir(self.folder(who))
            .timeout(CMD_TIMEOUT);
        cmd
    }

    /// `agora work ARGS` as `who`, with no expectation set yet.
    fn work(&self, who: &str, args: &[&str]) -> Assert {
        let mut full = vec!["work"];
        full.extend_from_slice(args);
        self.command(who).args(&full).assert()
    }

    /// alice bootstraps the owner, then turns off the machine gates in this home. The idle, memory and load readings
    /// are the host's, so with the defaults a job could be held before its process exists; the tests need the
    /// admission decision to depend on the queue alone. `--load-factor` is huge because the load gate only exists
    /// on Linux and cannot be switched off.
    fn init_owner(&self) {
        self.command("alice")
            .args(["init", "--owner"])
            .assert()
            .success();
        self.command("alice")
            .args([
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
            ])
            .assert()
            .success();
    }

    /// alice bootstraps the owner, bob registers as a worker.
    fn bootstrap(&self) {
        self.init_owner();
        self.command("bob").args(["init"]).assert().success();
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Best effort: an env that never started a daemon has nothing to stop.
        let _ = self.command("alice").arg("stop").output();
    }
}

fn stdout_of(assert: &Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
}

fn stderr_of(assert: &Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
}

fn json_of(assert: &Assert) -> Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("stdout is one JSON document")
}

/// Reads `work show ID --json` until the task reports `state`, for at most `STATE_WAIT`. A job is started by the
/// daemon, so the test observes its state before acting on it instead of assuming how far it has got.
fn await_state(env: &Env, id: &str, state: &str) {
    let deadline = Instant::now() + STATE_WAIT;
    loop {
        let doc = json_of(&env.work("alice", &["show", id, "--json"]).success());
        if doc["task"]["state"].as_str() == Some(state) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "task {id} did not reach {state:?} within {STATE_WAIT:?}: {doc}"
        );
        thread::sleep(STATE_POLL);
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The digest `work show` prints on its `digest: <hex>` line.
fn digest_of_show(shown: &str) -> String {
    let digest = shown
        .lines()
        .find_map(|line| line.strip_prefix("digest: "))
        .expect("show prints a digest line")
        .trim()
        .to_owned();
    assert!(is_hex64(&digest), "not a SHA-256 hex digest: {digest:?}");
    digest
}

fn fixture(name: &str, kind: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../agora-sched/tests/fixtures/ordering")
        .join(format!("{name}.{kind}.json"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).expect("fixture is JSON")
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("an array of strings")
        .iter()
        .map(|item| item.as_str().expect("a string").to_owned())
        .collect()
}

/// Runs `agora work ARGS --tasks FILE` for one ordering fixture (its `args` and its `tasks` document) in `env`.
fn run_given_tasks(env: &Env, name: &str) -> Assert {
    let input = fixture(name, "input");
    let tasks_file = env.folders.path().join(format!("{name}.tasks.json"));
    fs::write(&tasks_file, input["tasks"].to_string()).expect("write the tasks document");
    let mut args = vec!["work".to_owned()];
    args.extend(string_list(&input["args"]));
    args.push("--tasks".to_owned());
    args.push(tasks_file.to_str().expect("utf-8 path").to_owned());
    env.command("alice").args(&args).assert()
}

#[test]
fn given_tasks_stdout_stderr_and_exit_code_match_each_fixture() {
    let env = Env::new();
    let mut mismatches = Vec::new();
    for name in GIVEN_TASKS_CASES {
        let expected = fixture(name, "expected");
        let want_code = expected["exit_code"].as_i64().expect("exit code");
        let want_stdout = expected["stdout"].as_str().expect("stdout");
        let want_stderr = expected["stderr"].as_str().expect("stderr");
        let assert = run_given_tasks(&env, name);
        let output = assert.get_output();
        let code = output.status.code().map(i64::from);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if code != Some(want_code) || stdout != want_stdout || stderr != want_stderr {
            mismatches.push(format!(
                "{name}: exit {code:?} (want {want_code}); stdout {stdout:?} (want {want_stdout:?}); \
                 stderr {stderr:?} (want {want_stderr:?})"
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} fixture(s) differ:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

#[test]
fn given_tasks_order_and_plan_never_start_a_daemon() {
    let env = Env::new();
    run_given_tasks(&env, "order_critical_path").success();
    run_given_tasks(&env, "plan_two_agents_basic").success();
    assert!(
        !env.paths().socket.exists(),
        "order and plan with --tasks left a daemon socket behind"
    );
}

#[test]
fn run_of_a_succeeding_command_exits_0_and_reports_its_start_and_finish() {
    let env = Env::new();
    env.init_owner();
    let assert = env
        .work("alice", &["run", "--id", "ok1", "--", "true"])
        .code(0);
    let out = stdout_of(&assert);
    assert!(out.contains("work: started job ok1"), "{out}");
    assert!(out.contains("job ok1 finished with status 0"), "{out}");
}

#[test]
fn run_exits_with_the_status_of_its_command() {
    let env = Env::new();
    env.init_owner();
    let assert = env
        .work("alice", &["run", "--id", "e3", "--", "sh", "-c", "exit 3"])
        .code(3);
    assert!(
        stdout_of(&assert).contains("job e3 finished with status 3"),
        "{}",
        stdout_of(&assert)
    );
}

#[test]
fn run_of_a_missing_program_exits_127() {
    let env = Env::new();
    env.init_owner();
    let assert = env
        .work(
            "alice",
            &["run", "--id", "nf", "--", "agora-no-such-program-xyz"],
        )
        .code(127);
    assert!(
        stdout_of(&assert).contains("job nf finished with status 127"),
        "{}",
        stdout_of(&assert)
    );
}

#[test]
fn run_past_its_timeout_exits_124_and_wait_collects_the_status() {
    let env = Env::new();
    env.init_owner();
    // The job ends only when its gate file appears, so nothing but the CLI's 1 s timeout can end the run. The loop
    // gives up after about a minute, so a failing test does not leave the job running for long.
    let gate = env.folders.path().join("release-sl1");
    let gate_arg = gate.to_str().expect("utf-8 path");
    let gated = "i=0; while [ ! -e \"$0\" ] && [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done";
    let timed_out = env
        .work(
            "alice",
            &[
                "run",
                "--timeout",
                "1",
                "--id",
                "sl1",
                "--",
                "sh",
                "-c",
                gated,
                gate_arg,
            ],
        )
        .code(124);
    let out = stdout_of(&timed_out);
    // The 1 s timeout can fire before the daemon has admitted and started the job, so the report names the job as
    // not yet ended in whichever of its pending wordings applies. None of them says it finished.
    assert!(
        ["still running", "is starting", "still waiting for capacity"]
            .iter()
            .any(|pending| out.contains(&format!("job sl1 {pending}"))),
        "{out}"
    );
    assert!(out.contains("agora work wait sl1"), "{out}");

    fs::write(&gate, "").expect("release the job");
    let waited = env
        .work("alice", &["wait", "sl1", "--timeout", "60"])
        .code(0);
    assert!(
        stdout_of(&waited).contains("job sl1 finished with status 0"),
        "{}",
        stdout_of(&waited)
    );
}

#[test]
fn queued_command_cannot_run_before_its_claimant_accepts_it() {
    let env = Env::new();
    env.bootstrap();
    let made = env.folders.path().join("made-by-t1");
    let made_arg = made.to_str().expect("utf-8 path");
    env.work("alice", &["add", "--id", "t1", "--", "touch", made_arg])
        .success();
    env.work("bob", &["claim", "t1"]).success();
    env.work("bob", &["show", "t1"]).success();

    let assert = env.work("bob", &["run", "t1"]).code(1);
    assert!(
        stderr_of(&assert).contains("you have not accepted it"),
        "{}",
        stderr_of(&assert)
    );
    assert!(!made.exists(), "a command ran before its acceptance");
}

#[test]
fn queued_command_runs_after_the_claimant_accepts_its_digest() {
    let env = Env::new();
    env.bootstrap();
    let made = env.folders.path().join("made-by-t1");
    let made_arg = made.to_str().expect("utf-8 path");
    env.work("alice", &["add", "--id", "t1", "--", "touch", made_arg])
        .success();
    env.work("bob", &["claim", "t1"]).success();
    let digest = digest_of_show(&stdout_of(&env.work("bob", &["show", "t1"]).success()));

    env.work("bob", &["accept", "t1", "--digest", &digest])
        .success();
    let ran = env.work("bob", &["run", "t1"]).code(0);
    assert!(
        stdout_of(&ran).contains("job t1 finished with status 0"),
        "{}",
        stdout_of(&ran)
    );
    assert!(
        made.is_file(),
        "the accepted command did not create its file"
    );
}

#[test]
fn accept_with_a_digest_that_does_not_match_is_refused_and_nothing_is_accepted() {
    let env = Env::new();
    env.bootstrap();
    let made = env.folders.path().join("made-by-t1");
    let made_arg = made.to_str().expect("utf-8 path");
    env.work("alice", &["add", "--id", "t1", "--", "touch", made_arg])
        .success();
    env.work("bob", &["claim", "t1"]).success();

    let wrong = "0".repeat(64);
    let refused = env
        .work("bob", &["accept", "t1", "--digest", &wrong])
        .code(1);
    assert!(
        stderr_of(&refused).contains("the digest does not match"),
        "{}",
        stderr_of(&refused)
    );
    env.work("bob", &["run", "t1"]).code(1);
    assert!(
        !made.exists(),
        "a refused acceptance still let the command run"
    );
}

#[test]
fn author_cannot_accept_its_own_command_by_default() {
    let env = Env::new();
    env.bootstrap();
    let made = env.folders.path().join("made-by-t2");
    let made_arg = made.to_str().expect("utf-8 path");
    env.work("alice", &["add", "--id", "t2", "--", "touch", made_arg])
        .success();
    env.work("alice", &["claim", "t2"]).success();
    let digest = digest_of_show(&stdout_of(&env.work("alice", &["show", "t2"]).success()));

    let refused = env
        .work("alice", &["accept", "t2", "--digest", &digest])
        .code(1);
    assert!(
        stderr_of(&refused)
            .contains("a command must be accepted by an identity other than its author"),
        "{}",
        stderr_of(&refused)
    );
}

#[test]
fn next_with_nothing_queued_exits_1_and_says_nothing() {
    let env = Env::new();
    env.init_owner();
    let assert = env.work("alice", &["next"]).code(1);
    assert!(
        stdout_of(&assert).contains("nothing"),
        "{}",
        stdout_of(&assert)
    );
}

#[test]
fn cancel_stops_a_running_job_and_wait_reports_it_cancelled_with_status_143() {
    let env = Env::new();
    env.init_owner();
    // The 1 s timeout hands the run back while the job still runs; `sleep 60` cannot end before the cancel, which
    // follows the state poll, and it bounds how long a failing test leaves the job behind.
    env.work(
        "alice",
        &["run", "--timeout", "1", "--id", "c1", "--", "sleep", "60"],
    )
    .code(124);
    await_state(&env, "c1", "running");
    env.work("alice", &["cancel", "c1"]).success();

    let assert = env
        .work("alice", &["wait", "c1", "--timeout", "60", "--json"])
        .code(143);
    let doc = json_of(&assert);
    assert_eq!(doc["status"], 143, "{doc}");
    assert_eq!(doc["tasks"][0]["state"], "cancelled", "{doc}");
}

#[test]
fn status_json_has_machine_running_waiting_and_counts() {
    let env = Env::new();
    env.init_owner();
    let doc = json_of(&env.work("alice", &["status", "--json"]).success());
    for key in ["machine", "running", "waiting", "counts"] {
        assert!(doc.get(key).is_some(), "status --json lacks {key:?}: {doc}");
    }
}

#[test]
fn shim_forwards_order_with_a_task_to_the_binary() {
    let env = Env::new();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/agora-work");
    let assert = Command::new(&script)
        .env("AGORA_BIN", assert_cmd::cargo::cargo_bin("agora"))
        .env("AGORA_HOME", env.home.path())
        .args(["order", "--task", "id=a"])
        .timeout(CMD_TIMEOUT)
        .assert()
        .code(0);
    assert_eq!(stdout_of(&assert), "1. a (other)\n");
}

#[test]
fn wait_without_a_condition_is_a_usage_error_exit_2() {
    let env = Env::new();
    let assert = env.work("alice", &["wait"]).code(2);
    assert!(
        stderr_of(&assert).contains("wait for one thing"),
        "{}",
        stderr_of(&assert)
    );
}

#[test]
fn accept_without_a_digest_is_a_clap_usage_error_exit_2() {
    let env = Env::new();
    let assert = env.work("alice", &["accept", "t1"]).code(2);
    assert!(
        stderr_of(&assert).contains("--digest"),
        "{}",
        stderr_of(&assert)
    );
}
