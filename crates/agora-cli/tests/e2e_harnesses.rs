//! Real-harness smoke tests. `agora install` writes the Agora MCP server and hooks into a
//! temporary config directory, then the harness runs a prompt that calls the `whoami` tool.
//! The tests check that the tool answered with alice's pseudonym and that a hook ran.
//!
//! Both tests are ignored by default. Run them with `--ignored --nocapture` (docs/HARNESSES.md
//! section 6). A missing binary, or a missing input, skips its half with an `eprintln!` and
//! passes. Neither harness's real config directory is touched: each gets a temporary one through
//! its environment variable. The Codex auth file is copied and never printed.

use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// One `agora` command: the install and the identity lookup.
const AGORA_LIMIT: Duration = Duration::from_secs(60);
/// One Codex turn: a model call and the MCP tool call.
const CODEX_LIMIT: Duration = Duration::from_secs(180);
/// One Copilot prompt.
const COPILOT_LIMIT: Duration = Duration::from_secs(120);
/// Substrings (lowercase) that name an account problem in a failed Copilot run.
const AUTH_MARKERS: [&str; 4] = ["403", "not authorized", "subscription", "login"];
const CODEX_PROMPT: &str =
    "Call the agora whoami tool and print the pseudonym it returns, nothing else.";
const COPILOT_PROMPT: &str = "Call the agora whoami tool and print the pseudonym";

#[test]
#[ignore = "real harness smoke: run with --ignored (see docs/HARNESSES.md §6)"]
fn codex_exec_loads_agora_tools_and_runs_hooks() {
    if !on_path("codex") {
        eprintln!("SKIP: codex is not on PATH");
        return;
    }
    let Some(source) = codex_auth_source() else {
        eprintln!("SKIP: AGORA_E2E_CODEX_AUTH is unset or is not a file");
        return;
    };
    let root = temp_root();
    let home = root.path().join("home");
    let codex_home = root.path().join("codex");
    let agora_home = root.path().join("agora");
    let proj = root.path().join("proj");
    for dir in [&home, &codex_home, &proj] {
        fs::create_dir_all(dir).expect("create temp dir");
    }
    // Declared after `root`: both guards drop before the temporary directory is removed.
    let _auth = AuthCopy::create(&source, codex_home.join("auth.json")).expect("copy codex auth");
    let _daemon = Daemon {
        home: agora_home.clone(),
        proj: proj.clone(),
    };

    let mut install = agora(
        &agora_home,
        &proj,
        &["install", "--client", "codex", "--trust-hooks"],
    );
    install.env("HOME", &home).env("CODEX_HOME", &codex_home);
    must_succeed(install, "agora install --client codex", AGORA_LIMIT);
    let who = identity(&agora_home, &proj);

    let mut codex = base("codex", &agora_home, &proj);
    codex
        .env("HOME", &home)
        .env("CODEX_HOME", &codex_home)
        .args(["exec", "--skip-git-repo-check", "-C"])
        .arg(&proj)
        .args([
            "-c",
            "mcp_servers.agora.default_tools_approval_mode=\"approve\"",
        ])
        .arg(CODEX_PROMPT);
    let out = must_succeed(codex, "codex exec", CODEX_LIMIT);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&who.pseudonym),
        "codex stdout does not show pseudonym {}: {stdout}",
        who.pseudonym
    );
    assert!(
        hook_beat_exists(&agora_home, "codex", &who.pubkey),
        "no hook heartbeat (hook_inject or stop_continue) under {}",
        agora_home.display()
    );
}

#[test]
#[ignore = "real harness smoke: run with --ignored (see docs/HARNESSES.md §6)"]
fn copilot_prompt_loads_agora_tools() {
    if !on_path("copilot") {
        eprintln!("SKIP: copilot is not on PATH");
        return;
    }
    let root = temp_root();
    let config = root.path().join("copilot");
    let xdg = root.path().join("xdg");
    let agora_home = root.path().join("agora");
    let proj = root.path().join("proj");
    for dir in [&config, &xdg, &proj] {
        fs::create_dir_all(dir).expect("create temp dir");
    }
    let _daemon = Daemon {
        home: agora_home.clone(),
        proj: proj.clone(),
    };

    // HOME is left alone so the Copilot keychain login still works. COPILOT_HOME and
    // XDG_CONFIG_HOME point at the temporary directory, so ~/.copilot is not read or written.
    let mut install = agora(&agora_home, &proj, &["install", "--client", "copilot-cli"]);
    install
        .env("COPILOT_HOME", &config)
        .env("XDG_CONFIG_HOME", &xdg);
    must_succeed(install, "agora install --client copilot-cli", AGORA_LIMIT);
    let who = identity(&agora_home, &proj);

    let mut copilot = base("copilot", &agora_home, &proj);
    copilot
        .env("COPILOT_HOME", &config)
        .env("XDG_CONFIG_HOME", &xdg)
        .args(["-p", COPILOT_PROMPT, "--allow-all-tools"]);
    let out = run_bounded(copilot, COPILOT_LIMIT).unwrap_or_else(|e| panic!("copilot -p: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.contains(&who.pseudonym) && auth_failure(&out) {
        eprintln!("SKIP: Copilot account not entitled (the run failed on authentication)");
        return;
    }
    assert!(
        out.status.success(),
        "copilot -p exited with {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(&who.pseudonym),
        "copilot stdout does not show pseudonym {}: {stdout}",
        who.pseudonym
    );
}

/// Alice's identity for the folder `proj`.
struct Identity {
    pubkey: String,
    pseudonym: String,
}

/// `agora whoami --json` in `proj`. It creates the identity on first use.
fn identity(home: &Path, proj: &Path) -> Identity {
    let out = must_succeed(
        agora(home, proj, &["whoami", "--json"]),
        "agora whoami",
        AGORA_LIMIT,
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("whoami prints one JSON document");
    Identity {
        pubkey: doc["pubkey"].as_str().expect("pubkey string").to_owned(),
        pseudonym: doc["pseudonym"]
            .as_str()
            .expect("pseudonym string")
            .to_owned(),
    }
}

/// True when a hook of harness slug `harness` has beaten for the identity `pubkey` (hex) under the
/// Agora home `home`.
fn hook_beat_exists(home: &Path, harness: &str, pubkey: &str) -> bool {
    let dir = home
        .join("delivery")
        .join("heartbeats")
        .join(pubkey)
        .join(harness);
    ["hook_inject", "stop_continue"]
        .iter()
        .any(|mechanism| dir.join(mechanism).is_file())
}

/// True when the failed Copilot run names an account problem on stdout or stderr.
fn auth_failure(out: &Output) -> bool {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .to_lowercase();
    AUTH_MARKERS.iter().any(|marker| text.contains(marker))
}

/// A `Command` for `program` run from `cwd` against the Agora home `home`. Variables that would
/// change the identity or how the daemon starts are removed, as the hooks tests do.
fn base(program: impl AsRef<OsStr>, home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.env("AGORA_HOME", home)
        .current_dir(cwd)
        .env_remove("AGORA_FOLDER")
        .env_remove("AGORA_NO_SPAWN")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CLAUDE_PROJECT_DIR");
    cmd
}

/// The `agora` binary under test, run as the identity of `cwd`.
fn agora(home: &Path, cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = base(env!("CARGO_BIN_EXE_agora"), home, cwd);
    cmd.args(args);
    cmd
}

/// True when `bin` names an existing file in a `PATH` directory.
fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(bin).is_file()))
}

/// The Codex auth file named by `AGORA_E2E_CODEX_AUTH`, when it is set and is a file.
fn codex_auth_source() -> Option<PathBuf> {
    std::env::var_os("AGORA_E2E_CODEX_AUTH")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

/// A temporary root under /tmp. The daemon socket path must fit in 104 bytes, so the name stays
/// short.
fn temp_root() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ag")
        .tempdir_in("/tmp")
        .expect("temp root")
}

/// Stops the daemon that the test's `agora` commands started. Runs on success and on failure.
struct Daemon {
    home: PathBuf,
    proj: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Best effort: a run that never started a daemon has nothing to stop.
        let _ = agora(&self.home, &self.proj, &["stop"]).output();
    }
}

/// A copy of the Codex auth file, created with mode 0600 and removed on drop, whatever the
/// outcome of the test.
struct AuthCopy(PathBuf);

impl AuthCopy {
    fn create(source: &Path, dest: PathBuf) -> std::io::Result<Self> {
        let copy = Self(dest);
        let bytes = fs::read(source)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&copy.0)?
            .write_all(&bytes)?;
        Ok(copy)
    }
}

impl Drop for AuthCopy {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Prints `cmd` (program and arguments only; environment values can hold paths and secrets),
/// then returns its output. A run still going after `limit` is killed and reported as an error.
fn run_bounded(mut cmd: Command, limit: Duration) -> Result<Output, String> {
    eprintln!("running: {}", describe(&cmd));
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;
    let pid = child.id();
    let (done, waited) = mpsc::channel();
    // The waiter owns the child and drains both pipes, so the receive below can time out.
    thread::spawn(move || {
        let _ = done.send(child.wait_with_output());
    });
    match waited.recv_timeout(limit) {
        Ok(output) => output.map_err(|e| format!("wait failed: {e}")),
        Err(_) => {
            // The waiter reaps the killed child and exits on its own.
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
            Err(format!(
                "timed out after {}s and was killed",
                limit.as_secs()
            ))
        }
    }
}

/// `run_bounded`, requiring exit status 0; otherwise panics with `what` and stderr.
fn must_succeed(cmd: Command, what: &str, limit: Duration) -> Output {
    let output = run_bounded(cmd, limit).unwrap_or_else(|e| panic!("{what}: {e}"));
    assert!(
        output.status.success(),
        "{what} exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn describe(cmd: &Command) -> String {
    let args: Vec<String> = cmd
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    format!("{} {}", cmd.get_program().to_string_lossy(), args.join(" "))
}
