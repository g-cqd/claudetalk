//! `agora run-agent <harness>`: start an agent harness under an owned-session bridge
//! (`agora_bridge`) that delivers unread inbox messages into it (SPEC 5 and 6).

use crate::install::probe::is_executable_file;
use crate::{RunAgentArgs, RunAgentCmd};
use agora_bridge::acp::{self, AcpOptions};
use agora_bridge::agy::{self, AgyOptions};
use agora_bridge::codex::{self, CodexOptions};
use agora_bridge::opencode::{self, OpencodeOptions};
use agora_bridge::{BridgeError, SessionReady};
use serde::Serialize;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use tokio::signal::unix::SignalKind;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Resolve `--cwd` (default: the current directory, canonicalized; it is also the identity
/// folder), build the harness options, run the bridge until the agent exits or SIGINT/SIGTERM
/// (a `CancellationToken`), print one JSON line `{"harness":..,"session":..,"folder":..}` once the
/// session id is known, log to stderr, and return exit 0 on a clean end, non-zero on error (the
/// error printed to stderr as `agora run-agent: ..`). Records nothing in capabilities.json
/// (liveness = the `OwnedSessionApi` heartbeat the bridge writes).
///
/// Exit 2: a bad `--cwd`, `--bin` or `--conversation`, or a binary that is not on `PATH`.
/// Exit 1: the bridge failed.
pub fn run(home: PathBuf, args: RunAgentArgs) -> ExitCode {
    init_tracing();
    let harness = harness_name(&args.cmd);
    let cwd = match resolve_cwd(cwd_arg(&args.cmd)) {
        Ok(cwd) => cwd,
        Err(e) => return usage_error(&e),
    };
    let launch = match plan(&home, args.cmd, cwd.clone()) {
        Ok(launch) => launch,
        Err(e) => return usage_error(&e),
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("agora run-agent: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = rt.block_on(serve(harness, launch, home, cwd));
    // A blocked worker thread cannot be cancelled, so do not wait for it at exit.
    rt.shutdown_timeout(Duration::from_millis(200));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agora run-agent: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The bridge options of one harness, with paths resolved and binaries found.
enum Launch {
    Codex(CodexOptions),
    Acp(AcpOptions),
    Opencode(OpencodeOptions),
    Agy(AgyOptions),
}

/// The subcommand's name, which is also the `harness` field of the session line.
fn harness_name(cmd: &RunAgentCmd) -> &'static str {
    match cmd {
        RunAgentCmd::Codex { .. } => "codex",
        RunAgentCmd::Copilot { .. } => "copilot",
        RunAgentCmd::Opencode { .. } => "opencode",
        RunAgentCmd::Agy { .. } => "agy",
    }
}

/// The `--cwd` of any subcommand (`None` when not given).
fn cwd_arg(cmd: &RunAgentCmd) -> Option<&PathBuf> {
    match cmd {
        RunAgentCmd::Codex { cwd, .. }
        | RunAgentCmd::Copilot { cwd, .. }
        | RunAgentCmd::Opencode { cwd, .. }
        | RunAgentCmd::Agy { cwd, .. } => cwd.as_ref(),
    }
}

/// `requested` (default: the current directory), canonicalized. It must be a directory.
fn resolve_cwd(requested: Option<&PathBuf>) -> Result<PathBuf, String> {
    let path = match requested {
        Some(path) => path.clone(),
        None => std::env::current_dir().map_err(|e| format!("current directory: {e}"))?,
    };
    let canonical =
        std::fs::canonicalize(&path).map_err(|e| format!("--cwd {}: {e}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!("--cwd {}: not a directory", path.display()));
    }
    Ok(canonical)
}

/// `requested` (canonicalized, must be an executable file), else `name` found on `PATH`.
fn resolve_bin(requested: Option<&PathBuf>, name: &str) -> Result<PathBuf, String> {
    let Some(bin) = requested else {
        return find_on_path(name, std::env::var_os("PATH"))
            .ok_or_else(|| format!("{name} not found on PATH (pass --bin PATH)"));
    };
    let canonical =
        std::fs::canonicalize(bin).map_err(|e| format!("--bin {}: {e}", bin.display()))?;
    if is_executable_file(&canonical) {
        Ok(canonical)
    } else {
        Err(format!("--bin {}: not an executable file", bin.display()))
    }
}

/// The first executable file `name` in the `PATH` directories. Empty entries are skipped (they
/// would mean the current directory).
fn find_on_path(name: &str, path: Option<OsString>) -> Option<PathBuf> {
    let path = path?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

/// Build the bridge options for `cmd` with the identity folder `cwd` (already resolved).
fn plan(home: &Path, cmd: RunAgentCmd, cwd: PathBuf) -> Result<Launch, String> {
    Ok(match cmd {
        RunAgentCmd::Codex {
            thread,
            prompt,
            model,
            sandbox,
            approval,
            no_approve_agora_tools,
            bin,
            ..
        } => {
            let bin = resolve_bin(bin.as_ref(), "codex")?;
            let mut opts = CodexOptions::new(bin, cwd);
            opts.thread = thread;
            opts.prompt = prompt;
            opts.model = model;
            opts.sandbox = sandbox;
            opts.approval = approval;
            opts.approve_agora_tools = !no_approve_agora_tools;
            Launch::Codex(opts)
        }
        RunAgentCmd::Copilot {
            session,
            prompt,
            allow_agora_tools,
            bin,
            ..
        } => {
            let bin = resolve_bin(bin.as_ref(), "copilot")?;
            // The MCP server that copilot starts is this same binary.
            let exe = std::env::current_exe()
                .map_err(|e| format!("cannot locate the agora binary: {e}"))?;
            Launch::Acp(AcpOptions {
                bin,
                cwd,
                session,
                prompt,
                allow_tools: allow_agora_tools,
                exe,
                home: home.to_path_buf(),
            })
        }
        RunAgentCmd::Opencode {
            url,
            session,
            password_env,
            prompt,
            ..
        } => {
            let mut opts = OpencodeOptions::new(cwd);
            if let Some(url) = url {
                opts.url = url;
            }
            opts.session = session;
            opts.password_env = password_env;
            opts.prompt = prompt;
            Launch::Opencode(opts)
        }
        RunAgentCmd::Agy {
            conversation,
            bin,
            timeout_secs,
            ..
        } => {
            if conversation.trim().is_empty() {
                return Err("--conversation must not be empty".into());
            }
            let bin = resolve_bin(bin.as_ref(), "agy")?;
            let mut opts = AgyOptions::new(bin, cwd, conversation);
            opts.timeout = Duration::from_secs(timeout_secs);
            Launch::Agy(opts)
        }
    })
}

/// Run the bridge for `launch` until it ends or a stop signal arrives. The session line is
/// printed by a task that has finished by the time this returns.
async fn serve(
    harness: &'static str,
    launch: Launch,
    home: PathBuf,
    folder: PathBuf,
) -> Result<(), BridgeError> {
    let shutdown = CancellationToken::new();
    let signals = spawn_stop_signal(shutdown.clone());
    let (ready, session) = oneshot::channel::<String>();
    let printer = tokio::spawn(print_session(harness, session, folder));
    let result = bridge(launch, home, shutdown, ready).await;
    signals.abort();
    let _ = printer.await;
    result
}

/// Dispatch to the harness's bridge `run`.
async fn bridge(
    launch: Launch,
    home: PathBuf,
    shutdown: CancellationToken,
    ready: SessionReady,
) -> Result<(), BridgeError> {
    let ready = Some(ready);
    match launch {
        Launch::Codex(opts) => codex::run(opts, home, shutdown, ready).await,
        Launch::Acp(opts) => acp::run(opts, shutdown, ready).await,
        Launch::Opencode(opts) => opencode::run(opts, home, shutdown, ready).await,
        Launch::Agy(opts) => agy::run(opts, home, shutdown, ready).await,
    }
}

/// Waits for the session id and prints its line. Prints nothing when the bridge ended before
/// it knew the id.
async fn print_session(harness: &'static str, session: oneshot::Receiver<String>, folder: PathBuf) {
    let Ok(session) = session.await else {
        return;
    };
    print_line(&session_line(harness, &session, &folder));
}

/// Writes `line` and a newline to stdout and flushes. Write errors are ignored: stdout may be a
/// closed pipe, and the bridge keeps running regardless.
fn print_line(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The one-line JSON object `{"harness":..,"session":..,"folder":..}` (fields in this order).
fn session_line(harness: &str, session: &str, folder: &Path) -> String {
    #[derive(Serialize)]
    struct Line<'a> {
        harness: &'a str,
        session: &'a str,
        folder: &'a str,
    }
    let folder = folder.to_string_lossy();
    // Serializing three strings cannot fail.
    serde_json::to_string(&Line {
        harness,
        session,
        folder: &folder,
    })
    .unwrap_or_default()
}

/// Cancels `shutdown` on the first SIGINT or SIGTERM. A signal that cannot be registered never
/// fires, so the other one still works.
fn spawn_stop_signal(shutdown: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            () = wait_for_signal(SignalKind::interrupt()) => {}
            () = wait_for_signal(SignalKind::terminate()) => {}
        }
        shutdown.cancel();
    })
}

async fn wait_for_signal(kind: SignalKind) {
    match tokio::signal::unix::signal(kind) {
        Ok(mut stream) => {
            stream.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}

/// Prints `agora run-agent: msg` and returns exit 2 (a bad argument or missing binary).
fn usage_error(msg: &str) -> ExitCode {
    eprintln!("agora run-agent: {msg}");
    ExitCode::from(2)
}

/// Tracing to stderr (filter env AGORA_LOG, default "warn"), as `agora mcp` does. stdout carries
/// only the session line.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AGORA_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    // try_init: a subscriber installed earlier in this process is kept as it is.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::os::unix::fs::PermissionsExt;

    fn write_file(path: &Path, mode: u32) {
        std::fs::write(path, "#!/bin/sh\n").expect("write file");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    #[test]
    fn harness_name_matches_each_subcommand() {
        let codex = RunAgentCmd::Codex {
            cwd: None,
            thread: None,
            prompt: None,
            model: None,
            sandbox: "workspace-write".into(),
            approval: "never".into(),
            no_approve_agora_tools: false,
            bin: None,
        };
        let copilot = RunAgentCmd::Copilot {
            cwd: None,
            session: None,
            prompt: None,
            allow_agora_tools: false,
            bin: None,
        };
        let opencode = RunAgentCmd::Opencode {
            url: None,
            session: None,
            cwd: None,
            password_env: None,
            prompt: None,
        };
        let agy = RunAgentCmd::Agy {
            conversation: "c1".into(),
            cwd: None,
            bin: None,
            timeout_secs: 600,
        };
        assert_eq!(harness_name(&codex), "codex");
        assert_eq!(harness_name(&copilot), "copilot");
        assert_eq!(harness_name(&opencode), "opencode");
        assert_eq!(harness_name(&agy), "agy");
    }

    #[test]
    fn session_line_is_one_json_object_in_field_order() {
        let line = session_line("codex", "thr_1", Path::new("/work/proj"));
        assert_eq!(
            line,
            r#"{"harness":"codex","session":"thr_1","folder":"/work/proj"}"#
        );
    }

    #[test]
    fn session_line_escapes_quotes_backslashes_and_newlines() {
        let line = session_line("opencode", "s\"1\\\n2", Path::new("/tmp/a \"b\""));
        assert!(!line.contains('\n'), "one line only: {line}");
        let value: Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(value["harness"], "opencode");
        assert_eq!(value["session"], "s\"1\\\n2");
        assert_eq!(value["folder"], "/tmp/a \"b\"");
    }

    #[test]
    fn find_on_path_takes_executable_files_only() {
        let dir = tempfile::tempdir().expect("temp dir");
        let exec = dir.path().join("fake-agent");
        write_file(&exec, 0o755);
        write_file(&dir.path().join("plain-agent"), 0o644);
        let path = std::env::join_paths([dir.path()]).expect("join");
        assert_eq!(
            find_on_path("fake-agent", Some(path.clone())),
            Some(exec),
            "an executable file is found"
        );
        assert_eq!(
            find_on_path("plain-agent", Some(path.clone())),
            None,
            "a non-executable file is skipped"
        );
        assert_eq!(find_on_path("missing", Some(path)), None);
        assert_eq!(
            find_on_path("fake-agent", None),
            None,
            "no PATH finds nothing"
        );
    }
}
