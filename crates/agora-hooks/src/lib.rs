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

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::time::Duration;

use schema::HookInput;

pub mod context;
pub mod daemon;
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
}

impl HookOptions {
    /// The defaults above for `home`; `quick` false; `session` and `folder` `None`.
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

/// Parse stdin (`input::read_input`; `Poll` ignores stdin and uses a default `HookInput`),
/// build a current-thread tokio runtime, run the kind's `run` (rewake without the budget
/// timeout; every other kind inside `tokio::time::timeout(budget_ms)`), all inside
/// `std::panic::catch_unwind`. Any `Err`, timeout or panic -> `HookOutcome::silent()`. The
/// exit code is decided by `normalise`.
pub fn run(kind: HookKind, opts: &HookOptions, stdin: &mut dyn std::io::Read) -> HookOutcome {
    let guarded = std::panic::catch_unwind(AssertUnwindSafe(|| run_guarded(kind, opts, stdin)));
    normalise(kind, guarded.unwrap_or_else(|_| HookOutcome::silent()))
}

/// Bound on the runtime's shutdown: a blocking task (a file lock) must not keep the hook alive.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(100);

/// The body of `run`, which may panic; `run` turns a panic into silence.
fn run_guarded(kind: HookKind, opts: &HookOptions, stdin: &mut dyn std::io::Read) -> HookOutcome {
    let hook_input = match kind {
        // The mod's poll never reads stdin.
        HookKind::Poll => HookInput::default(),
        _ => match input::read_input(stdin) {
            Ok(parsed) => parsed,
            Err(_) => return HookOutcome::silent(),
        },
    };
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
    ran.unwrap_or_else(|_| HookOutcome::silent())
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
