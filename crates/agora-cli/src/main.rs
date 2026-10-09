//! `agora`: command-line client and daemon launcher.
//!
//! Every command except `daemon` talks to the daemon over `$AGORA_HOME/agora.sock`,
//! auto-spawning it when absent. The CLI holds the private key (derived per folder
//! from the machine seed), signs every op locally and wraps/unwraps channel keys
//! locally: the daemon never sees a private key or a channel key.

mod args_board;
mod args_chat;
mod args_daemon;
mod args_hook;
mod args_install;
mod args_org;
mod args_run_agent;
mod args_sched;
mod args_work;
mod cmd_board;
mod cmd_chat;
mod cmd_hook;
mod cmd_mcp;
mod cmd_org;
mod cmd_run_agent;
mod cmd_sched;
mod cmd_work;
mod cmd_work_run;
mod install;

pub use args_board::*;
pub use args_chat::*;
pub use args_daemon::*;
pub use args_hook::*;
pub use args_install::*;
pub use args_org::*;
pub use args_run_agent::*;
pub use args_sched::*;
pub use args_work::*;

pub use agora_client::ClientError as CliError;
pub use agora_client::{Ctx, Out};

use agora_client::identity;
use agora_daemon::{DaemonConfig, Paths};
use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
    name = "agora",
    version,
    about = "Signed, encrypted chat and work board for agents"
)]
pub struct Cli {
    /// Data directory (default: $AGORA_HOME, else ~/.agora).
    #[arg(long, global = true, env = "AGORA_HOME")]
    pub home: Option<PathBuf>,
    /// Print one JSON document on stdout instead of text.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// Parsed once at startup, so the size of the largest variant does not matter.
#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Cmd {
    /// Run the daemon in the foreground (logs to stderr; AGORA_LOG sets the filter).
    Daemon(DaemonArgs),
    /// MCP server for agent harnesses: stdio by default, `--http ADDR` for HTTP,
    /// `token` to manage the HTTP bearer tokens.
    Mcp(McpArgs),
    /// Stop the running daemon (no-op when none runs; never spawns one).
    Stop,
    /// Create this folder's identity and register it (--owner: bootstrap the owner).
    Init(InitArgs),
    /// Show this folder's identity and role.
    Whoami,
    /// Post a message.
    Chat(ChatArgs),
    /// Read a channel.
    Read(ReadArgs),
    /// Unacked messages across your channels (--wait SECS blocks up to 55 s).
    Inbox(InboxArgs),
    /// Mark a channel read up to SEQ.
    Ack(AckArgs),
    /// Add or remove a reaction.
    React(ReactArgs),
    /// Download an attachment blob.
    Fetch(FetchArgs),
    #[command(subcommand)]
    Channel(ChannelCmd),
    #[command(subcommand)]
    Card(CardCmd),
    #[command(subcommand)]
    Sprint(SprintCmd),
    #[command(subcommand)]
    Note(NoteCmd),
    #[command(subcommand)]
    Role(RoleCmd),
    /// Revoke an identity (controller or owner).
    Revoke(RevokeArgs),
    /// Claude Code hook entry points (used by the plugin; always exit 0 except rewake's wake).
    Hook(HookArgs),
    /// Install the Agora MCP server, hooks and instructions into agent clients (`--client`).
    Install(InstallArgs),
    /// Undo `install`: remove only what `install` added.
    Uninstall(InstallArgs),
    /// Run an agent harness under an owned-session bridge that delivers inbox messages into it
    /// (opt-in: the command itself is the opt-in).
    RunAgent(RunAgentArgs),
    /// The shared work queue: add, claim, run and wait for jobs on this machine. Prints the work.py
    /// texts; exit codes follow the job (124 = the wait timed out, 130 = interrupted).
    Work(WorkArgs),
    /// Capacity pools and path/pool leases.
    #[command(subcommand)]
    Pool(PoolCmd),
    /// Manual quiet windows for the job scheduler.
    #[command(subcommand)]
    Quiet(QuietCmd),
    /// Team and lane policies (stale, escalate, reclaim, WIP).
    #[command(subcommand)]
    Policy(PolicyCmd),
    /// What the watchdog sees: stale cards, expiring leases, notices, WIP, orphaned jobs.
    Watchdog,
    /// Flow metrics: lead and cycle time, WIP, blocked time, queue waits.
    Metrics {
        #[arg(long)]
        team: Option<String>,
        #[arg(long)]
        lane: Option<String>,
        /// Only work finished after this time (ms since epoch).
        #[arg(long)]
        since_ms: Option<i64>,
    },
    /// Notices the reconciler and escalations addressed to you.
    Notices {
        /// Only notices with seq greater than this.
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Internal: supervise one job (started by the daemon).
    #[command(name = "_supervise", hide = true)]
    Supervise {
        #[arg(long)]
        job_dir: PathBuf,
    },
}

/// The data directory: `--home`/`AGORA_HOME` when set. An empty value, or an unexpanded
/// placeholder such as `${AGORA_HOME:-}` (a plugin `.mcp.json` variable), counts as unset.
fn home_of(cli: &Cli) -> PathBuf {
    match &cli.home {
        Some(h) if !h.as_os_str().is_empty() && !h.to_string_lossy().contains("${") => h.clone(),
        _ => DaemonConfig::default_home(),
    }
}

fn run_daemon(home: PathBuf, args: DaemonArgs) -> Result<(), String> {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AGORA_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    let config = DaemonConfig::load(&home).map_err(|e| e.to_string())?;
    match args.mcp_http {
        Some(addr) => run_daemon_with_mcp(config, &home, addr, args),
        None => agora_daemon::run_blocking(config).map_err(|e| e.to_string()),
    }
}

/// `agora daemon --mcp-http ADDR`: the daemon and the MCP HTTP front-end in one process, on
/// one runtime. The front-end talks to the daemon like any client would (the daemon's own
/// shutdown token stops both). If the front-end fails, the daemon is stopped too.
fn run_daemon_with_mcp(
    config: DaemonConfig,
    home: &Path,
    addr: SocketAddr,
    args: DaemonArgs,
) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async move {
        // Bind the MCP port and check the token file before the daemon touches its socket,
        // so a refused address or token file leaves no daemon behind.
        let listener = agora_mcp::http::bind(addr, args.allow_remote)
            .await
            .map_err(|e| e.to_string())?;
        let paths = Paths::new(home);
        let opts = cmd_mcp::http_options(&paths, args.token_file, args.allow_origin);
        cmd_mcp::check_tokens(&opts.token_file)?;
        let daemon = agora_daemon::Daemon::bind(config)
            .await
            .map_err(|e| e.to_string())?;
        let token = daemon.shutdown_token();
        let local = listener.local_addr().map_err(|e| e.to_string())?;
        eprintln!("agora daemon: MCP over HTTP on http://{local}/mcp");
        let engine = agora_mcp::engine::Engine::new(agora_mcp::engine::EngineConfig::http());
        let serving = agora_mcp::http::serve(listener, opts, engine, token.clone());
        let mcp = async move {
            let result = serving.await;
            if result.is_err() {
                token.cancel();
            }
            result.map_err(|e| e.to_string())
        };
        let (daemon_result, mcp_result) = tokio::join!(daemon.run(), mcp);
        daemon_result.map_err(|e| e.to_string())?;
        mcp_result
    })
}

async fn dispatch(ctx: &Ctx, cmd: Cmd) -> Result<Out, CliError> {
    match cmd {
        Cmd::Daemon(_)
        | Cmd::Mcp(_)
        | Cmd::Hook(_)
        | Cmd::Install(_)
        | Cmd::Uninstall(_)
        | Cmd::RunAgent(_)
        | Cmd::Work(_)
        | Cmd::Supervise { .. } => Err(CliError::Usage(
            "daemon, mcp, hook, install, uninstall, run-agent, work and _supervise are handled before dispatch"
                .into(),
        )),
        Cmd::Stop => cmd_org::stop(ctx).await,
        Cmd::Init(a) => cmd_org::init(ctx, a).await,
        Cmd::Whoami => cmd_org::whoami(ctx).await,
        Cmd::Chat(a) => cmd_chat::chat(ctx, a).await,
        Cmd::Read(a) => cmd_chat::read(ctx, a).await,
        Cmd::Inbox(a) => cmd_chat::inbox(ctx, a).await,
        Cmd::Ack(a) => cmd_chat::ack(ctx, a).await,
        Cmd::React(a) => cmd_chat::react(ctx, a).await,
        Cmd::Fetch(a) => cmd_chat::fetch(ctx, a).await,
        Cmd::Channel(c) => cmd_org::channel(ctx, c).await,
        Cmd::Role(c) => cmd_org::role(ctx, c).await,
        Cmd::Revoke(a) => cmd_org::revoke(ctx, a).await,
        Cmd::Card(c) => cmd_board::card(ctx, c).await,
        Cmd::Sprint(c) => cmd_board::sprint(ctx, c).await,
        Cmd::Note(c) => cmd_board::note(ctx, c).await,
        Cmd::Pool(c) => cmd_sched::pool(ctx, c).await,
        Cmd::Quiet(c) => cmd_sched::quiet(ctx, c).await,
        Cmd::Policy(c) => cmd_sched::policy(ctx, c).await,
        Cmd::Watchdog => cmd_sched::watchdog(ctx).await,
        Cmd::Metrics {
            team,
            lane,
            since_ms,
        } => cmd_sched::metrics(ctx, team, lane, since_ms).await,
        Cmd::Notices { after, limit } => cmd_sched::notices(ctx, after, limit).await,
    }
}

/// `agora _supervise --job-dir D`: the same supervisor as the `agora-supervise` binary.
fn supervise(job_dir: &Path) -> ExitCode {
    let args = ["--job-dir".to_string(), job_dir.display().to_string()];
    let code = agora_run::supervise::main_from_args(&args);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Exit code of a long-running server command (`daemon`, `mcp`): its error goes to stderr.
fn finish_server(name: &str, result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agora: {name}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Whether the invocation is a `hook` one: the first argument after argv[0] that is not one of
/// the global options `--json`, `--home <v>` or `--home=<v>` equals `hook`.
fn is_hook_invocation(args: &[OsString]) -> bool {
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        if arg.as_os_str() == "--json" || arg.as_encoded_bytes().starts_with(b"--home=") {
            continue;
        }
        if arg.as_os_str() == "--home" {
            rest.next();
            continue;
        }
        return arg.as_os_str() == "hook";
    }
    false
}

/// The arguments after `work` of an `agora [--home H] [--json] work (order|plan) ...` invocation that
/// carries `--tasks` or `--task`: answered before clap runs, without a daemon or an identity. A `--json`
/// given before `work` is appended, because the work.py emulation reads it among its own arguments.
fn given_tasks_args(args: &[OsString]) -> Option<Vec<String>> {
    let mut json_first = false;
    let mut rest = args.iter().skip(1);
    loop {
        let arg = rest.next()?;
        if arg.as_os_str() == "--json" {
            json_first = true;
        } else if arg.as_os_str() == "--home" {
            rest.next()?;
        } else if !arg.as_encoded_bytes().starts_with(b"--home=") {
            if arg.as_os_str() != "work" {
                return None;
            }
            break;
        }
    }
    let mut after: Vec<String> = rest
        .map(|a| a.to_str().map(str::to_owned))
        .collect::<Option<_>>()?;
    if !matches!(after.first().map(String::as_str), Some("order" | "plan"))
        || !agora_sched::compat::is_given_tasks(&after)
    {
        return None;
    }
    if json_first && !after.iter().any(|a| a == "--json") {
        after.push("--json".into());
    }
    Some(after)
}

/// A parse error of a hook invocation exits 0 silently (clap's own exit 2 would block a Stop or
/// UserPromptSubmit). Help and version still print, as they do for every command.
fn hook_parse_error_is_silent(kind: clap::error::ErrorKind, args: &[OsString]) -> bool {
    use clap::error::ErrorKind;
    is_hook_invocation(args) && !matches!(kind, ErrorKind::DisplayHelp | ErrorKind::DisplayVersion)
}

fn main() -> ExitCode {
    let raw_args: Vec<OsString> = std::env::args_os().collect();
    if let Some(args) = given_tasks_args(&raw_args) {
        return cmd_work::given_tasks(&args);
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let args: Vec<OsString> = std::env::args_os().collect();
            if hook_parse_error_is_silent(e.kind(), &args) {
                return ExitCode::SUCCESS;
            }
            e.exit();
        }
    };
    let home = home_of(&cli);
    let json = cli.json;
    let result = match cli.cmd {
        Cmd::Daemon(a) => return finish_server("daemon", run_daemon(home, a)),
        Cmd::Mcp(McpArgs {
            cmd: Some(McpCmd::Token(c)),
            token_file,
            http: None,
            ..
        }) => cmd_mcp::token(home, json, token_file, c),
        Cmd::Mcp(McpArgs { cmd: Some(_), .. }) => Err(CliError::Usage(
            "--http cannot be combined with `mcp token`".into(),
        )),
        Cmd::Mcp(a) => return finish_server("mcp", cmd_mcp::run(home, a)),
        Cmd::Hook(a) => return cmd_hook::run(home, a),
        Cmd::RunAgent(a) => return cmd_run_agent::run(home, a),
        Cmd::Work(a) => return cmd_work::run(home, json, a),
        Cmd::Supervise { job_dir } => return supervise(&job_dir),
        Cmd::Install(a) => install::install(home, a),
        Cmd::Uninstall(a) => install::uninstall(home, a),
        cmd => (|| -> Result<Out, CliError> {
            let paths = Paths::new(&home);
            let (identity, label) = identity::load_identity(&paths)?;
            let ctx = Ctx {
                paths,
                identity,
                label,
            };
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(dispatch(&ctx, cmd))
        })(),
    };
    match result {
        Ok(out) => {
            if json {
                println!("{}", out.json);
            } else if !out.text.is_empty() {
                println!("{}", out.text);
            }
            // A result that reports `"ok": false` (a multi-client install where some client
            // failed) is printed in full and still fails the command.
            if out.json.get("ok") == Some(&serde_json::Value::Bool(false)) {
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"error": {"code": e.code(), "message": e.to_string()}})
                );
            } else {
                eprintln!("agora: error: {e}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
