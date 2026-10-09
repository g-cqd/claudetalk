//! Claude Code hook entry points for Agora delivery. Each subcommand of `agora hook` runs one
//! entry point here. They bring inbox messages into a Claude session at the points where Claude
//! Code runs hooks, and prove their liveness with heartbeat files so the MCP shim knows which
//! delivery rungs are armed.
//!
//! | `agora hook`    | Claude Code event                                       | Output                                                              |
//! |-----------------|---------------------------------------------------------|---------------------------------------------------------------------|
//! | `context`       | UserPromptSubmit, PostToolUse, PostToolBatch            | stdout `hookSpecificOutput.additionalContext`, exit 0               |
//! | `stop`          | Stop                                                    | stdout `{"decision":"block","reason":..}` while messages are unread |
//! | `rewake`        | Stop (asyncRewake, long wait); SessionStart (`--quick`) | stderr: the rendered messages, exit 2 (wakes an idle session)       |
//! | `session-start` | SessionStart                                            | stdout `hookSpecificOutput.additionalContext` (identity, unread)    |
//! | `poll`          | none: called by the plugin mod                          | stdout one JSON line `{"count":N,"text":..}`                        |
//!
//! Every hook fails open: any outcome other than the output above is silent (empty stdout and
//! stderr, exit 0). No hook acks a message on the agent's behalf, and none spawns the daemon.
//!
//! Delivery model: every hook rung only triggers delivery of UNACKED inbox messages; nothing
//! acks on the agent's behalf. The hook-family rungs (`context`, `stop`, `rewake`, `poll`)
//! share ONE per-session dedupe cursor (the highest seq already delivered to that Claude
//! session) in a per-session state file under an exclusive lock (`state`), so a message is
//! delivered once per session by whichever rung sees it first.
//!
//! Hard rules for every hook: read at most 256 KiB of stdin; never spawn the daemon
//! (`Client::connect_no_spawn`); a current-thread tokio runtime; the whole run bounded by
//! `budget_ms` (except rewake's wait); ANY error, timeout or panic yields `HookOutcome::silent()`
//! (exit 0, empty stdout, empty stderr), because exit code 2 would block a Stop or a
//! UserPromptSubmit. Only `rewake` exits 2, on purpose, to wake an idle session. Nothing is
//! printed except the protocol output; secrets and prompt text are never logged.
//!
//! Dialects (M6): `agora hook <kind> --dialect codex|copilot|copilot-vscode|agy|cursor [--event E]`
//! runs the same entry points for another harness. The `dialect` module converts that harness's
//! stdin into the Claude-shaped input before the entry point runs, and the entry point's
//! Claude-shaped stdout into the harness's own output after it. A non-Claude dialect runs only
//! `context`, `stop` and `session-start`; `rewake` and `poll` are silent for it. Such a run acts for
//! the payload's `cwd` unless `AGORA_FOLDER` names a folder (see `dialect_folder`).

use std::ffi::OsString;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::time::Duration;

use schema::HookInput;

pub mod context;
pub mod daemon;
pub mod dialect;
pub mod input;
pub mod poll;
pub mod render;
pub mod rewake;
pub mod schema;
pub mod session_start;
pub mod state;
pub mod stop;

/// Default bound (ms) of a whole hook run, rewake's wait excepted. Env `AGORA_HOOK_BUDGET_MS`
/// overrides it (read by the CLI).
pub const DEFAULT_BUDGET_MS: u64 = 3_000;
/// Default total long-poll time (ms) of `rewake`: just under the hook's 3600 s timeout.
pub const DEFAULT_MAX_WAIT_MS: u64 = 3_540_000;
/// Budget (ms) of `rewake --quick`'s single non-waiting fetch (min with `budget_ms`).
pub const QUICK_BUDGET_MS: u64 = 1_500;
/// Default cap on consecutive Stop blocks (`stop_blocks`) before `stop` lets Claude stop.
pub const DEFAULT_MAX_BLOCKS: u32 = 8;
/// Default cap (chars) of the text a hook hands to Claude Code (its own cap is 10,000).
pub const DEFAULT_MAX_CONTEXT_CHARS: usize = 9_000;

/// Which hook entry point to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    Rewake,
    Context,
    Stop,
    SessionStart,
    Poll,
}

/// Everything a hook run needs besides stdin.
#[derive(Debug, Clone)]
pub struct HookOptions {
    /// The agora home (`$AGORA_HOME`).
    pub home: PathBuf,
    pub budget_ms: u64,
    pub max_wait_ms: u64,
    /// `rewake --quick` (the SessionStart variant).
    pub quick: bool,
    pub max_blocks: u32,
    pub max_context_chars: usize,
    /// `poll --session`.
    pub session: Option<String>,
    /// `poll --folder`.
    pub folder: Option<PathBuf>,
    /// `--dialect`: whose native stdin/stdout shape this run speaks (default `Claude`).
    pub dialect: dialect::Dialect,
    /// `--event`: the native event name passed on the command line (validated by the CLI).
    pub event: Option<String>,
}

impl HookOptions {
    /// The defaults above for `home`; `quick` false; `session`, `folder` and `event` `None`; `dialect` `Claude`.
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            budget_ms: DEFAULT_BUDGET_MS,
            max_wait_ms: DEFAULT_MAX_WAIT_MS,
            quick: false,
            max_blocks: DEFAULT_MAX_BLOCKS,
            max_context_chars: DEFAULT_MAX_CONTEXT_CHARS,
            session: None,
            folder: None,
            dialect: dialect::Dialect::Claude,
            event: None,
        }
    }
}

/// What a hook prints and how it exits. `exit_code` is 0, or 2 for rewake's wake.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl HookOutcome {
    /// Nothing on either stream, exit 0.
    pub fn silent() -> Self {
        Self::default()
    }

    /// `s` on stdout, exit 0.
    pub fn stdout(s: String) -> Self {
        Self {
            stdout: s,
            ..Self::default()
        }
    }

    /// `stderr` text and exit 2: Claude Code wakes the session and shows the text.
    pub fn wake(stderr: String) -> Self {
        Self {
            stderr,
            exit_code: 2,
            ..Self::default()
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("input: {0}")]
    Input(String),
    #[error(transparent)]
    Client(#[from] agora_client::ClientError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("timed out")]
    Timeout,
}

/// Read and convert stdin (`run_guarded`), build a current-thread tokio runtime, run the kind's
/// `run` (rewake without the budget timeout; every other kind inside
/// `tokio::time::timeout(budget_ms)`), all inside `std::panic::catch_unwind`. Any `Err`, timeout
/// or panic -> `HookOutcome::silent()`. The exit code is decided by `normalise`, which runs last.
pub fn run(kind: HookKind, opts: &HookOptions, stdin: &mut dyn std::io::Read) -> HookOutcome {
    let guarded = std::panic::catch_unwind(AssertUnwindSafe(|| run_guarded(kind, opts, stdin)));
    normalise(kind, guarded.unwrap_or_else(|_| HookOutcome::silent()))
}

/// Bound on the runtime's shutdown: a blocking task (a file lock) must not keep the hook alive.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);

/// The folder a non-Claude dialect run acts for when `AGORA_FOLDER` does not name one: the native
/// payload's `cwd`, when `dialect` is not Claude, `agora_folder_env` is unset, empty or contains an
/// unexpanded `${` placeholder, and `cwd` is non-empty. `None` otherwise, and the run keeps the
/// usual resolution (`opts.folder`, then `AGORA_FOLDER`, `CLAUDE_PROJECT_DIR`, the payload `cwd`).
/// Pure, so the rule is tested without the process environment.
fn dialect_folder(
    dialect: dialect::Dialect,
    agora_folder_env: Option<OsString>,
    cwd: Option<&str>,
) -> Option<PathBuf> {
    if dialect == dialect::Dialect::Claude {
        return None;
    }
    let env_names_folder =
        agora_folder_env.is_some_and(|v| !v.is_empty() && !v.to_string_lossy().contains("${"));
    if env_names_folder {
        return None;
    }
    cwd.filter(|c| !c.is_empty()).map(PathBuf::from)
}

/// The body of `run`, which may panic; `run` turns a panic into silence. For a non-Claude
/// dialect, `Rewake` and `Poll` are silent before stdin is read. Otherwise `Poll` uses a default
/// `HookInput` and every other kind reads the bytes (`input::read_bytes`) and converts them
/// (`dialect::to_input`; an error is silent). A non-Claude run whose `AGORA_FOLDER` does not name
/// a folder runs with the payload's `cwd` as its folder (`dialect_folder`). After the dispatch,
/// `dialect::to_output` converts the outcome to the dialect's native output (identity for Claude).
fn run_guarded(kind: HookKind, opts: &HookOptions, stdin: &mut dyn std::io::Read) -> HookOutcome {
    let claude = opts.dialect == dialect::Dialect::Claude;
    if !claude && matches!(kind, HookKind::Rewake | HookKind::Poll) {
        return HookOutcome::silent();
    }
    let hook_input = match kind {
        // The mod's poll never reads stdin.
        HookKind::Poll => HookInput::default(),
        _ => {
            let Ok(bytes) = input::read_bytes(stdin) else {
                return HookOutcome::silent();
            };
            match dialect::to_input(opts.dialect, opts.event.as_deref(), &bytes) {
                Ok(parsed) => parsed,
                Err(_) => return HookOutcome::silent(),
            }
        }
    };
    let scoped = dialect_folder(
        opts.dialect,
        std::env::var_os("AGORA_FOLDER"),
        hook_input.cwd.as_deref(),
    )
    .map(|folder| HookOptions {
        folder: Some(folder),
        ..opts.clone()
    });
    let opts = scoped.as_ref().unwrap_or(opts);
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return HookOutcome::silent();
    };
    // The runtime stays owned here, so it is shut down even when the future panicked.
    let ran = std::panic::catch_unwind(AssertUnwindSafe(|| {
        rt.block_on(dispatch(kind, opts, &hook_input))
    }));
    rt.shutdown_timeout(SHUTDOWN_TIMEOUT);
    let outcome = ran.unwrap_or_else(|_| HookOutcome::silent());
    dialect::to_output(opts.dialect, kind, &hook_input, outcome)
}

/// Runs the kind's entry point inside its budget. Timeout or error -> silent.
async fn dispatch(kind: HookKind, opts: &HookOptions, hook_input: &HookInput) -> HookOutcome {
    let work = async {
        match kind {
            HookKind::Rewake => rewake::run(opts, hook_input).await,
            HookKind::Context => context::run(opts, hook_input).await,
            HookKind::Stop => stop::run(opts, hook_input).await,
            HookKind::SessionStart => session_start::run(opts, hook_input).await,
            HookKind::Poll => poll::run(opts, hook_input).await,
        }
    };
    let budget = Duration::from_millis(budget_ms(kind, opts));
    match tokio::time::timeout(budget, work).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_)) | Err(_) => HookOutcome::silent(),
    }
}

/// The time bound of one run in ms. `rewake --quick` is capped by `QUICK_BUDGET_MS`; long
/// `rewake` is bounded by its wait plus the budget (a safety net, the wait itself is the bound);
/// every other kind by `budget_ms`.
fn budget_ms(kind: HookKind, opts: &HookOptions) -> u64 {
    match kind {
        HookKind::Rewake if opts.quick => opts.budget_ms.min(QUICK_BUDGET_MS),
        HookKind::Rewake => opts.max_wait_ms.saturating_add(opts.budget_ms),
        _ => opts.budget_ms,
    }
}

/// The single place that decides the exit code. Exit 2 (a wake) survives only for
/// `HookKind::Rewake` with non-empty stderr. Every other outcome becomes exit 0 with stderr
/// cleared and stdout kept, so no other hook can block a Stop or a UserPromptSubmit.
fn normalise(kind: HookKind, outcome: HookOutcome) -> HookOutcome {
    if kind == HookKind::Rewake && outcome.exit_code == 2 && !outcome.stderr.is_empty() {
        return outcome;
    }
    HookOutcome {
        stderr: String::new(),
        exit_code: 0,
        ..outcome
    }
}

#[cfg(test)]
mod lib_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    const USER_PROMPT: &str = r#"{"session_id":"e773b56f-0c1d-4a2b-9f3e-6a7b8c9d0e1f","cwd":"/tmp/agora-test","hook_event_name":"UserPromptSubmit","prompt":"hello"}"#;

    /// Fails loudly if read, and records that a read was attempted.
    struct PanickingReader<'a>(&'a AtomicBool);

    impl Read for PanickingReader<'_> {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.store(true, Ordering::SeqCst);
            panic!("stdin must not be read");
        }
    }

    #[test]
    fn garbage_stdin_is_silent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = HookOptions::new(dir.path().to_path_buf());
        let out = run(HookKind::Context, &opts, &mut &b"not json at all"[..]);
        assert_eq!(out, HookOutcome::silent());
    }

    #[test]
    fn oversized_stdin_is_silent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = HookOptions::new(dir.path().to_path_buf());
        let mut body = br#"{"hook_event_name":"UserPromptSubmit","prompt":""#.to_vec();
        body.extend(std::iter::repeat_n(b'a', 300 * 1024));
        body.extend_from_slice(br#""}"#);
        let out = run(HookKind::Context, &opts, &mut body.as_slice());
        assert_eq!(out, HookOutcome::silent());
    }

    #[test]
    fn valid_prompt_with_missing_home_is_silent_and_quick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = HookOptions::new(dir.path().join("does-not-exist"));
        let started = Instant::now();
        let out = run(HookKind::Context, &opts, &mut USER_PROMPT.as_bytes());
        let elapsed = started.elapsed();
        assert_eq!(out, HookOutcome::silent());
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    }

    #[test]
    fn poll_never_reads_stdin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opts = HookOptions::new(dir.path().to_path_buf());
        let read = AtomicBool::new(false);
        let out = run(HookKind::Poll, &opts, &mut PanickingReader(&read));
        assert_eq!(out, HookOutcome::silent());
        assert!(!read.load(Ordering::SeqCst), "poll read stdin");
    }

    #[test]
    fn wake_with_empty_stderr_becomes_silent() {
        let out = normalise(HookKind::Rewake, HookOutcome::wake(String::new()));
        assert_eq!(out, HookOutcome::silent());
    }

    #[test]
    fn other_exit_codes_become_zero_and_clear_stderr() {
        for code in [1, 3, -1] {
            let out = normalise(
                HookKind::Rewake,
                HookOutcome {
                    stdout: "out".into(),
                    stderr: "boom".into(),
                    exit_code: code,
                },
            );
            assert_eq!(
                out,
                HookOutcome {
                    stdout: "out".into(),
                    stderr: String::new(),
                    exit_code: 0,
                }
            );
        }
    }

    #[test]
    fn exit_zero_clears_stderr_and_keeps_stdout() {
        let out = normalise(
            HookKind::Stop,
            HookOutcome {
                stdout: "out".into(),
                stderr: "note".into(),
                exit_code: 0,
            },
        );
        assert_eq!(out, HookOutcome::stdout("out".into()));
    }

    #[test]
    fn rewake_wake_keeps_stderr_and_exits_two() {
        let out = normalise(HookKind::Rewake, HookOutcome::wake("x".into()));
        assert_eq!(out.exit_code, 2);
        assert_eq!(out.stderr, "x");
        assert_eq!(out, HookOutcome::wake("x".into()));
    }

    #[test]
    fn context_wake_becomes_exit_zero() {
        let out = normalise(HookKind::Context, HookOutcome::wake("x".into()));
        assert_eq!(out.exit_code, 0);
        assert!(out.stderr.is_empty());
        assert_eq!(out, HookOutcome::silent());
    }

    #[test]
    fn stop_wake_becomes_exit_zero() {
        let out = normalise(HookKind::Stop, HookOutcome::wake("x".into()));
        assert_eq!(out.exit_code, 0);
        assert!(out.stderr.is_empty());
        assert_eq!(out, HookOutcome::silent());
    }

    #[test]
    fn non_rewake_wake_keeps_stdout() {
        let out = normalise(
            HookKind::Context,
            HookOutcome {
                stdout: "out".into(),
                stderr: "x".into(),
                exit_code: 2,
            },
        );
        assert_eq!(out, HookOutcome::stdout("out".into()));
    }

    #[test]
    fn rewake_budgets() {
        let mut opts = HookOptions::new(PathBuf::from("/nonexistent"));
        opts.quick = true;
        assert_eq!(budget_ms(HookKind::Rewake, &opts), QUICK_BUDGET_MS);
        opts.quick = false;
        assert_eq!(
            budget_ms(HookKind::Rewake, &opts),
            DEFAULT_MAX_WAIT_MS + DEFAULT_BUDGET_MS
        );
        assert_eq!(budget_ms(HookKind::Stop, &opts), DEFAULT_BUDGET_MS);
    }
}
