//! `agora hook <sub>`: maps the clap arguments onto `agora_hooks` and prints the protocol output.

use crate::{HookArgs, HookCmd};
use agora_hooks::{HookKind, HookOptions, HookOutcome};
use std::io::{Cursor, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// Environment variable that overrides the hook budget (milliseconds).
const BUDGET_ENV: &str = "AGORA_HOOK_BUDGET_MS";
const MIN_BUDGET_MS: u64 = 1;
const MAX_BUDGET_MS: u64 = 600_000;
const MIN_CONTEXT_CHARS: usize = 200;
const MAX_CONTEXT_CHARS: usize = 9_500;
const MAX_BLOCKS_CAP: u32 = 100;

/// Build `agora_hooks::HookOptions` from `args` (`budget_ms` from env `AGORA_HOOK_BUDGET_MS`
/// when it parses as a positive integer; `--max-wait-secs`, `--quick`, `--max-blocks`,
/// `--max-chars`, `--session`, `--folder` as given), read stdin (an empty reader when stdin is
/// a terminal), call `agora_hooks::run`, write `stdout` / `stderr` of the outcome exactly
/// (no added newline) and return the exit code: 0 or 2 only, anything else -> 0. Never prints
/// anything else, never panics on a closed pipe. Stdin is read on a helper thread, so a writer
/// that never closes the pipe ends the hook silently at the budget (exit 0, no output).
pub fn run(home: PathBuf, args: HookArgs) -> ExitCode {
    // agora_hooks::run catches panics; keep the default hook from writing to stderr.
    std::panic::set_hook(Box::new(|_| {}));
    let budget_env = std::env::var(BUDGET_ENV).ok();
    let (kind, mut opts) = options(home, &args.cmd, budget_env.as_deref());
    let t0 = Instant::now();
    let stdin = if kind == HookKind::Poll || std::io::stdin().is_terminal() {
        Vec::new()
    } else {
        let wait = stdin_wait_ms(kind, opts.quick, opts.budget_ms);
        match read_bounded(std::io::stdin(), Duration::from_millis(wait)) {
            Some(bytes) => bytes,
            None => return ExitCode::SUCCESS,
        }
    };
    opts.budget_ms = remaining_budget_ms(opts.budget_ms, millis_saturating(t0.elapsed()));
    let out = agora_hooks::run(kind, &opts, &mut Cursor::new(stdin));
    write_outcome(&out);
    ExitCode::from(exit_code_of(out.exit_code))
}

/// Read at most `MAX_STDIN_BYTES + 1` bytes from `r` on a helper thread, waiting at most
/// `timeout`. `None` on a read error or when the wait times out; the reader thread is then
/// abandoned and ends with the process. `agora_hooks::run` re-applies the size limit.
fn read_bounded<R: Read + Send + 'static>(r: R, timeout: Duration) -> Option<Vec<u8>> {
    let limit = u64::try_from(agora_hooks::input::MAX_STDIN_BYTES + 1).unwrap_or(u64::MAX);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .spawn(move || {
            let mut buf = Vec::new();
            let result = r.take(limit).read_to_end(&mut buf).map(|_| buf);
            // The receiver may already have timed out and gone; nothing to do then.
            let _ = tx.send(result);
        })
        .ok()?;
    rx.recv_timeout(timeout).ok()?.ok()
}

/// How long to wait for stdin. `rewake --quick` waits at most `QUICK_BUDGET_MS`; every other hook
/// waits the whole budget.
fn stdin_wait_ms(kind: HookKind, quick: bool, budget_ms: u64) -> u64 {
    if kind == HookKind::Rewake && quick {
        budget_ms.min(agora_hooks::QUICK_BUDGET_MS)
    } else {
        budget_ms
    }
}

/// The budget left for `agora_hooks::run` once `elapsed_ms` went to reading stdin. Never below 1 ms,
/// so the hook still gets a bounded, non-zero run.
fn remaining_budget_ms(budget_ms: u64, elapsed_ms: u64) -> u64 {
    budget_ms.saturating_sub(elapsed_ms).max(1)
}

/// Whole milliseconds of `d`, saturating at `u64::MAX`.
fn millis_saturating(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The hook kind and options for `cmd`; `budget_env` is the raw value of `AGORA_HOOK_BUDGET_MS`.
fn options(home: PathBuf, cmd: &HookCmd, budget_env: Option<&str>) -> (HookKind, HookOptions) {
    let mut opts = HookOptions::new(home);
    if let Some(ms) = budget_env.and_then(parse_budget) {
        opts.budget_ms = ms;
    }
    let kind = match cmd {
        HookCmd::Rewake {
            quick,
            max_wait_secs,
        } => {
            opts.quick = *quick;
            if let Some(secs) = *max_wait_secs {
                opts.max_wait_ms = secs.saturating_mul(1000);
            }
            HookKind::Rewake
        }
        HookCmd::Context { max_chars } => {
            set_max_chars(&mut opts, *max_chars);
            HookKind::Context
        }
        HookCmd::Stop {
            max_blocks,
            max_chars,
        } => {
            if let Some(n) = *max_blocks {
                opts.max_blocks = n.min(MAX_BLOCKS_CAP);
            }
            set_max_chars(&mut opts, *max_chars);
            HookKind::Stop
        }
        HookCmd::SessionStart => HookKind::SessionStart,
        HookCmd::Poll { session, folder } => {
            opts.session = Some(session.clone());
            opts.folder = Some(folder.clone());
            HookKind::Poll
        }
    };
    (kind, opts)
}

/// `AGORA_HOOK_BUDGET_MS`: an integer in `1..=600_000`; anything else is ignored.
fn parse_budget(raw: &str) -> Option<u64> {
    raw.parse::<u64>()
        .ok()
        .filter(|ms| (MIN_BUDGET_MS..=MAX_BUDGET_MS).contains(ms))
}

fn set_max_chars(opts: &mut HookOptions, max_chars: Option<usize>) {
    if let Some(n) = max_chars {
        opts.max_context_chars = n.clamp(MIN_CONTEXT_CHARS, MAX_CONTEXT_CHARS);
    }
}

/// Only 2 (a blocking error, used by `rewake`) is passed on; every other code becomes 0.
fn exit_code_of(code: i32) -> u8 {
    if code == 2 { 2 } else { 0 }
}

/// Write the protocol output exactly. A closed pipe or any other write error is ignored.
fn write_outcome(out: &HookOutcome) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(out.stdout.as_bytes());
    if !out.stdout.is_empty() && !out.stdout.ends_with('\n') {
        let _ = stdout.write_all(b"\n");
    }
    let _ = stdout.flush();
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(out.stderr.as_bytes());
    let _ = stderr.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use agora_hooks::{
        DEFAULT_BUDGET_MS, DEFAULT_MAX_BLOCKS, DEFAULT_MAX_CONTEXT_CHARS, DEFAULT_MAX_WAIT_MS,
    };

    fn opts_for(cmd: &HookCmd) -> (HookKind, HookOptions) {
        options(PathBuf::from("/tmp/agora-home-test"), cmd, None)
    }

    #[test]
    fn rewake_quick_keeps_default_wait() {
        let (kind, o) = opts_for(&HookCmd::Rewake {
            quick: true,
            max_wait_secs: None,
        });
        assert_eq!(kind, HookKind::Rewake);
        assert!(o.quick);
        assert_eq!(o.max_wait_ms, DEFAULT_MAX_WAIT_MS);
    }

    #[test]
    fn rewake_max_wait_secs_becomes_ms() {
        let (kind, o) = opts_for(&HookCmd::Rewake {
            quick: false,
            max_wait_secs: Some(5),
        });
        assert_eq!(kind, HookKind::Rewake);
        assert!(!o.quick);
        assert_eq!(o.max_wait_ms, 5_000);
    }

    #[test]
    fn rewake_max_wait_secs_saturates() {
        let (_, o) = opts_for(&HookCmd::Rewake {
            quick: false,
            max_wait_secs: Some(u64::MAX),
        });
        assert_eq!(o.max_wait_ms, u64::MAX);
    }

    #[test]
    fn context_defaults_when_no_max_chars() {
        let (kind, o) = opts_for(&HookCmd::Context { max_chars: None });
        assert_eq!(kind, HookKind::Context);
        assert_eq!(o.max_context_chars, DEFAULT_MAX_CONTEXT_CHARS);
    }

    #[test]
    fn context_max_chars_is_clamped() {
        let chars = |n| {
            opts_for(&HookCmd::Context { max_chars: Some(n) })
                .1
                .max_context_chars
        };
        assert_eq!(chars(50), 200);
        assert_eq!(chars(1_000), 1_000);
        assert_eq!(chars(20_000), 9_500);
    }

    #[test]
    fn stop_defaults() {
        let (kind, o) = opts_for(&HookCmd::Stop {
            max_blocks: None,
            max_chars: None,
        });
        assert_eq!(kind, HookKind::Stop);
        assert_eq!(o.max_blocks, DEFAULT_MAX_BLOCKS);
        assert_eq!(o.max_context_chars, DEFAULT_MAX_CONTEXT_CHARS);
    }

    #[test]
    fn stop_limits_are_clamped() {
        let (_, o) = opts_for(&HookCmd::Stop {
            max_blocks: Some(500),
            max_chars: Some(10),
        });
        assert_eq!(o.max_blocks, 100);
        assert_eq!(o.max_context_chars, 200);

        let (_, o) = opts_for(&HookCmd::Stop {
            max_blocks: Some(0),
            max_chars: None,
        });
        assert_eq!(o.max_blocks, 0);
    }

    #[test]
    fn session_start_maps_kind_and_keeps_defaults() {
        let (kind, o) = opts_for(&HookCmd::SessionStart);
        assert_eq!(kind, HookKind::SessionStart);
        assert_eq!(o.max_context_chars, DEFAULT_MAX_CONTEXT_CHARS);
        assert!(!o.quick);
    }

    #[test]
    fn poll_carries_session_and_folder() {
        let (kind, o) = opts_for(&HookCmd::Poll {
            session: "abc-123".to_string(),
            folder: PathBuf::from("/work/project"),
        });
        assert_eq!(kind, HookKind::Poll);
        assert_eq!(o.session.as_deref(), Some("abc-123"));
        assert_eq!(o.folder, Some(PathBuf::from("/work/project")));
    }

    #[test]
    fn budget_env_is_parsed_and_bounded() {
        let budget = |raw: Option<&str>| {
            options(PathBuf::from("/h"), &HookCmd::SessionStart, raw)
                .1
                .budget_ms
        };
        assert_eq!(budget(None), DEFAULT_BUDGET_MS);
        assert_eq!(budget(Some("garbage")), DEFAULT_BUDGET_MS);
        assert_eq!(budget(Some("")), DEFAULT_BUDGET_MS);
        assert_eq!(budget(Some("0")), DEFAULT_BUDGET_MS);
        assert_eq!(budget(Some("-5")), DEFAULT_BUDGET_MS);
        assert_eq!(budget(Some("600001")), DEFAULT_BUDGET_MS);
        assert_eq!(budget(Some("99999999999999999999999")), DEFAULT_BUDGET_MS);
        assert_eq!(budget(Some("1")), 1);
        assert_eq!(budget(Some("600000")), 600_000);
    }

    #[test]
    fn exit_code_only_passes_two() {
        assert_eq!(exit_code_of(2), 2);
        assert_eq!(exit_code_of(0), 0);
        assert_eq!(exit_code_of(1), 0);
        assert_eq!(exit_code_of(101), 0);
        assert_eq!(exit_code_of(-1), 0);
    }

    /// A reader that never produces data while its sender is alive.
    struct Blocked(std::sync::mpsc::Receiver<()>);

    impl Read for Blocked {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            // Returns only once the test drops the sender, which ends the abandoned thread.
            let _ = self.0.recv();
            Ok(0)
        }
    }

    struct Failing;

    impl Read for Failing {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("boom"))
        }
    }

    #[test]
    fn read_bounded_returns_data_then_eof() {
        let got = read_bounded(Cursor::new(b"{\"a\":1}".to_vec()), Duration::from_secs(5));
        assert_eq!(got.as_deref(), Some(&b"{\"a\":1}"[..]));
    }

    #[test]
    fn read_bounded_stops_at_the_size_limit() {
        let over = vec![b'x'; agora_hooks::input::MAX_STDIN_BYTES + 5];
        let got = read_bounded(Cursor::new(over), Duration::from_secs(5));
        assert_eq!(
            got.map(|b| b.len()),
            Some(agora_hooks::input::MAX_STDIN_BYTES + 1)
        );
    }

    #[test]
    fn read_bounded_gives_up_on_a_writer_that_never_closes() {
        let (keep, rx) = std::sync::mpsc::channel::<()>();
        let timeout = Duration::from_millis(100);
        let start = std::time::Instant::now();
        let got = read_bounded(Blocked(rx), timeout);
        assert_eq!(got, None);
        assert!(start.elapsed() < timeout + Duration::from_secs(1));
        drop(keep);
    }

    #[test]
    fn read_bounded_returns_none_on_read_error() {
        assert_eq!(read_bounded(Failing, Duration::from_secs(5)), None);
    }

    #[test]
    fn quick_rewake_waits_at_most_the_quick_budget_for_stdin() {
        assert_eq!(
            stdin_wait_ms(HookKind::Rewake, true, DEFAULT_BUDGET_MS),
            agora_hooks::QUICK_BUDGET_MS
        );
        assert_eq!(stdin_wait_ms(HookKind::Rewake, true, 1_000), 1_000);
        assert_eq!(stdin_wait_ms(HookKind::SessionStart, true, 3_000), 3_000);
    }

    #[test]
    fn other_hooks_and_long_rewake_wait_the_whole_budget_for_stdin() {
        assert_eq!(stdin_wait_ms(HookKind::Rewake, false, 3_000), 3_000);
        assert_eq!(stdin_wait_ms(HookKind::Context, false, 3_000), 3_000);
        assert_eq!(stdin_wait_ms(HookKind::Stop, false, 3_000), 3_000);
    }

    #[test]
    fn remaining_budget_subtracts_the_stdin_time() {
        assert_eq!(remaining_budget_ms(3_000, 400), 2_600);
        assert_eq!(remaining_budget_ms(3_000, 0), 3_000);
    }

    #[test]
    fn remaining_budget_never_drops_below_one_ms() {
        assert_eq!(remaining_budget_ms(3_000, 3_000), 1);
        assert_eq!(remaining_budget_ms(3_000, 5_000), 1);
        assert_eq!(remaining_budget_ms(0, 0), 1);
    }

    #[test]
    fn elapsed_and_budget_arithmetic_saturates_instead_of_wrapping() {
        assert_eq!(remaining_budget_ms(u64::MAX, 0), u64::MAX);
        assert_eq!(remaining_budget_ms(10, u64::MAX), 1);
        assert_eq!(millis_saturating(Duration::from_millis(1_500)), 1_500);
        assert_eq!(millis_saturating(Duration::MAX), u64::MAX);
    }
}
