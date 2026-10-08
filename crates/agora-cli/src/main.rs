//! `agora`: command-line client and daemon launcher.
//!
//! Every command except `daemon` talks to the daemon over `$AGORA_HOME/agora.sock`,
//! auto-spawning it when absent. The CLI holds the private key (derived per folder
//! from the machine seed), signs every op locally and wraps/unwraps channel keys
//! locally: the daemon never sees a private key or a channel key.

mod args_sched;
mod args_work;
mod cmd_board;
mod cmd_chat;
mod cmd_hook;
mod cmd_mcp;
mod cmd_org;
mod cmd_sched;
mod cmd_work;
mod cmd_work_run;
mod install;

pub use args_sched::*;
pub use args_work::*;

pub use agora_client::ClientError as CliError;
pub use agora_client::{Ctx, Out};

use agora_client::identity;
use agora_daemon::{DaemonConfig, Paths};
use clap::{Args, Parser, Subcommand};
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
    /// Install the Agora MCP server, hooks and mod into an agent client (M4: `--client claude`).
    Install(InstallArgs),
    /// Undo `install`: remove only what `install` added.
    Uninstall(InstallArgs),
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

/// Options of `agora daemon`.
#[derive(Args, Debug)]
pub struct DaemonArgs {
    /// Also serve MCP over HTTP on ADDR (e.g. 127.0.0.1:8765), inside the daemon process.
    #[arg(long, value_name = "ADDR")]
    pub mcp_http: Option<SocketAddr>,
    /// Allow --mcp-http to bind a non-loopback address (no TLS: put a TLS proxy in front).
    #[arg(long, requires = "mcp_http")]
    pub allow_remote: bool,
    /// Extra browser origin allowed to call the HTTP endpoint (repeatable).
    #[arg(long = "allow-origin", value_name = "URL", requires = "mcp_http")]
    pub allow_origin: Vec<String>,
    /// Bearer-token file for --mcp-http (default: $AGORA_HOME/mcp-tokens.json).
    #[arg(long, value_name = "PATH", requires = "mcp_http")]
    pub token_file: Option<PathBuf>,
}

/// Options of `agora mcp`: stdio unless `--http` is given.
#[derive(Args, Debug)]
pub struct McpArgs {
    #[command(subcommand)]
    pub cmd: Option<McpCmd>,
    /// Serve MCP over HTTP on ADDR (e.g. 127.0.0.1:8765) instead of stdio.
    #[arg(long, value_name = "ADDR")]
    pub http: Option<SocketAddr>,
    /// Allow --http to bind a non-loopback address (no TLS: put a TLS proxy in front).
    #[arg(long, requires = "http")]
    pub allow_remote: bool,
    /// Extra browser origin allowed to call the HTTP endpoint (repeatable).
    #[arg(long = "allow-origin", value_name = "URL", requires = "http")]
    pub allow_origin: Vec<String>,
    /// Bearer-token file for --http and `token` (default: $AGORA_HOME/mcp-tokens.json).
    #[arg(long, value_name = "PATH", global = true)]
    pub token_file: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum McpCmd {
    /// Manage HTTP bearer tokens.
    #[command(subcommand)]
    Token(TokenCmd),
}

#[derive(Subcommand, Debug)]
pub enum TokenCmd {
    /// Create a token (printed once) bound to a fresh server-held identity.
    Add {
        #[arg(long)]
        label: String,
    },
    /// List tokens (ids, labels, public keys; never secrets).
    List,
    /// Revoke a token by id.
    Revoke { id: String },
}

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Become the owner (only when no owner exists).
    #[arg(long)]
    pub owner: bool,
    /// Registration label (default: the folder name).
    #[arg(long)]
    pub label: Option<String>,
}

#[derive(Args, Debug)]
pub struct ChatArgs {
    pub channel: String,
    pub text: String,
    /// Reply in the thread of this message id.
    #[arg(long)]
    pub thread_of: Option<String>,
    /// Attach a file (repeatable). Encrypted channels get the file sealed first.
    #[arg(long)]
    pub attach: Vec<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ReadArgs {
    pub channel: String,
    /// Only messages with seq greater than this.
    #[arg(long, default_value_t = 0)]
    pub after: i64,
    #[arg(long, default_value_t = 50)]
    pub limit: i64,
}

#[derive(Args, Debug)]
pub struct InboxArgs {
    /// Wait up to SECS (max 55) for a message when the inbox is empty.
    #[arg(long)]
    pub wait: Option<u64>,
    /// Only messages with seq greater than this (cursor from a previous call).
    #[arg(long, default_value_t = 0)]
    pub after: i64,
    #[arg(long, default_value_t = 50)]
    pub limit: i64,
}

#[derive(Args, Debug)]
pub struct AckArgs {
    pub channel: String,
    pub seq: i64,
}

#[derive(Args, Debug)]
pub struct ReactArgs {
    /// Message id (or blob hash / card id with --kind).
    pub target: String,
    pub emoji: String,
    /// message | file | card
    #[arg(long, default_value = "message")]
    pub kind: String,
    #[arg(long)]
    pub remove: bool,
}

#[derive(Args, Debug)]
pub struct FetchArgs {
    pub hash: String,
    /// Output file.
    #[arg(long)]
    pub out: PathBuf,
    /// Decrypt with this channel's key (encrypted channels).
    #[arg(long)]
    pub channel: Option<String>,
    /// Key epoch of the message that carried the attachment (default: current).
    #[arg(long)]
    pub epoch: Option<i64>,
}

#[derive(Subcommand, Debug)]
pub enum ChannelCmd {
    Create {
        id: String,
        /// dm | group | thread | squad | team
        #[arg(long, default_value = "group")]
        kind: String,
        #[arg(long, default_value = "")]
        name: String,
        #[arg(long)]
        parent: Option<String>,
        /// Member public key (hex64), optionally suffixed ":admin". Repeatable.
        #[arg(long = "member")]
        members: Vec<String>,
        /// Create an end-to-end encrypted channel.
        #[arg(long)]
        encrypt: bool,
    },
    Add {
        channel: String,
        pk: String,
        #[arg(long)]
        admin: bool,
    },
    /// Remove a member (rotates the key of an encrypted channel).
    Remove {
        channel: String,
        pk: String,
    },
    Promote {
        channel: String,
        pk: String,
    },
    Demote {
        channel: String,
        pk: String,
    },
    /// New key epoch for all current members (required after a revoke).
    Rotate {
        channel: String,
    },
    List,
    Info {
        channel: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum CardCmd {
    Create {
        id: String,
        title: String,
        #[arg(long)]
        lane: Option<String>,
        /// Higher = more urgent.
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        value: Option<f64>,
        #[arg(long)]
        size: Option<f64>,
        #[arg(long)]
        team: Option<String>,
        /// Cards this one waits for.
        #[arg(long, num_args = 1..)]
        after: Vec<String>,
    },
    /// Move to a state: backlog ready claimed doing review done blocked ...
    Move {
        id: String,
        state: String,
    },
    Evidence {
        id: String,
        reference: String,
    },
    Approve {
        id: String,
    },
    Reclaim {
        id: String,
    },
    List {
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        lane: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: i64,
    },
    /// Change a card's priority, value, size, team, needs-owner flag or owner action.
    Update {
        id: String,
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        value: Option<f64>,
        #[arg(long)]
        size: Option<f64>,
        #[arg(long)]
        team: Option<String>,
        #[arg(long, num_args = 0..=1, default_missing_value = "true")]
        needs_owner: Option<bool>,
        #[arg(long)]
        owner_action: Option<String>,
    },
    /// Make a card wait for another (`--remove` drops the dependency).
    Dep {
        id: String,
        after: String,
        #[arg(long)]
        remove: bool,
    },
    /// Link a card to another card, a job, a message or a note.
    Link {
        id: String,
        /// re | blocks | relates | duplicates | caused_by | job | message | note
        rel: String,
        target: String,
        #[arg(long)]
        remove: bool,
    },
    /// Flag a card as needing its owner's attention and notify the escalation chain.
    Escalate {
        id: String,
        /// What the owner has to do.
        #[arg(long)]
        owner_action: String,
        /// Public keys to notify (default: the policy's chain, else the supervisors).
        #[arg(long, num_args = 1..)]
        to: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum SprintCmd {
    Create {
        id: String,
        name: String,
        #[arg(long, default_value = "")]
        goal: String,
        /// Start, ms since epoch.
        #[arg(long)]
        starts: i64,
        /// End, ms since epoch (must be after start).
        #[arg(long)]
        ends: i64,
    },
    Add {
        sprint: String,
        card: String,
    },
    Start {
        sprint: String,
    },
    Close {
        sprint: String,
    },
    Summary {
        sprint: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum NoteCmd {
    Publish {
        id: String,
        title: String,
        body: String,
        #[arg(long, default_value = "")]
        tags: String,
        #[arg(long)]
        supersedes: Option<String>,
    },
    Search {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
}

#[derive(Subcommand, Debug)]
pub enum RoleCmd {
    /// Grant ROLE (observer worker supervisor controller owner) to PK.
    Grant { pk: String, role: String },
}

#[derive(Args, Debug)]
pub struct RevokeArgs {
    pub pk: String,
}

/// Options of `agora hook`.
#[derive(Args, Debug)]
pub struct HookArgs {
    #[command(subcommand)]
    pub cmd: HookCmd,
}

#[derive(Subcommand, Debug)]
pub enum HookCmd {
    /// asyncRewake long-poll (Stop) or --quick check (SessionStart).
    Rewake {
        /// SessionStart variant: one short non-waiting check, no ownership.
        #[arg(long)]
        quick: bool,
        /// Stop waiting after SECS (default: just under the hook timeout).
        #[arg(long)]
        max_wait_secs: Option<u64>,
    },
    /// additionalContext for UserPromptSubmit / PostToolUse / PostToolBatch.
    Context {
        /// Cap on the characters handed to Claude Code.
        #[arg(long)]
        max_chars: Option<usize>,
    },
    /// Stop: block while the inbox has undelivered messages (capped).
    Stop {
        /// Most consecutive blocks before Claude may stop.
        #[arg(long)]
        max_blocks: Option<u32>,
        /// Cap on the characters handed to Claude Code.
        #[arg(long)]
        max_chars: Option<usize>,
    },
    /// SessionStart: identity, unread summary, instructions.
    SessionStart,
    /// Called by the plugin mod: one JSON line {count, text?}.
    Poll {
        /// Claude session id.
        #[arg(long)]
        session: String,
        /// Project folder whose identity polls.
        #[arg(long)]
        folder: PathBuf,
    },
}

/// Options of `agora install` / `agora uninstall`.
#[derive(Args, Debug)]
pub struct InstallArgs {
    /// Agent client to configure (M4: `claude`).
    #[arg(long)]
    pub client: String,
    /// user: ~/.claude; project: the current directory's .claude and .mcp.json.
    #[arg(long, value_enum, default_value_t = ScopeArg::User)]
    pub scope: ScopeArg,
    /// plugin: install the Agora plugin (MCP, hooks and mod); entries: plain MCP entry and hooks.
    #[arg(long, value_enum, default_value_t = ModeArg::Plugin)]
    pub mode: ModeArg,
    /// Probe the installed client's version and flags and record them in capabilities.json.
    #[arg(long)]
    pub probe: bool,
    /// Show what would change; write nothing.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeArg {
    User,
    Project,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeArg {
    Plugin,
    Entries,
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
        | Cmd::Work(_)
        | Cmd::Supervise { .. } => Err(CliError::Usage(
            "daemon, mcp, hook, install, uninstall, work and _supervise are handled before dispatch"
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
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    /// Parses `agora ARGS...` exactly as `main` does.
    fn parse(args: &[String]) -> Cli {
        Cli::try_parse_from(std::iter::once("agora".to_string()).chain(args.iter().cloned()))
            .expect("parses")
    }

    #[test]
    fn token_file_writes_to_its_file_before_or_after_the_token_subcommand() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dir.path().join("home");
        let file = dir.path().join("tokens.json");
        let f = file.display().to_string();
        let orders = [
            strings(&["mcp", "--token-file", &f, "token", "add", "--label", "l"]),
            strings(&["mcp", "token", "add", "--label", "l", "--token-file", &f]),
        ];
        for args in orders {
            let cli = parse(&args);
            let Cmd::Mcp(McpArgs {
                cmd: Some(McpCmd::Token(c)),
                token_file,
                http: None,
                ..
            }) = cli.cmd
            else {
                panic!("not a token command: {args:?}");
            };
            assert_eq!(token_file.as_deref(), Some(file.as_path()), "{args:?}");
            cmd_mcp::token(home.clone(), false, token_file, c).expect("token add");
            assert!(file.exists(), "{args:?}");
            std::fs::remove_file(&file).expect("reset");
        }
        assert!(!agora_mcp::tokens::default_path(&Paths::new(&home)).exists());
    }

    #[test]
    fn token_file_without_http_is_refused_by_the_stdio_server() {
        let dir = tempfile::tempdir().expect("temp dir");
        let f = dir.path().join("tokens.json").display().to_string();
        let cli = parse(&strings(&["mcp", "--token-file", f.as_str()]));
        let Cmd::Mcp(args) = cli.cmd else {
            panic!("not mcp");
        };
        let err = cmd_mcp::run(dir.path().to_path_buf(), args).expect_err("refused");
        assert_eq!(err, "--token-file needs --http");
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(|a| OsString::from(*a)).collect()
    }

    #[test]
    fn work_order_with_tasks_is_routed_before_parsing() {
        let some = |a: &[&str]| given_tasks_args(&os(a));
        assert_eq!(
            some(&["agora", "work", "order", "--task", "id=a"]),
            Some(strings(&["order", "--task", "id=a"]))
        );
        assert_eq!(
            some(&[
                "agora", "--home", "/h", "--json", "work", "plan", "--agents", "2", "--tasks", "-"
            ]),
            Some(strings(&[
                "plan", "--agents", "2", "--tasks", "-", "--json"
            ]))
        );
        assert_eq!(some(&["agora", "work", "order", "--policy", "wsjf"]), None);
    }

    #[test]
    fn only_work_order_and_plan_are_candidates_for_early_routing() {
        let some = |a: &[&str]| given_tasks_args(&os(a));
        assert_eq!(some(&["agora", "work", "add", "--task", "x"]), None);
        assert_eq!(
            some(&["agora", "chat", "work", "order", "--task", "a"]),
            None
        );
        assert_eq!(some(&["agora", "--home"]), None);
        assert_eq!(some(&["agora", "work"]), None);
        assert_eq!(some(&["agora"]), None);
    }

    #[test]
    fn work_commands_declare_their_flags() {
        for args in [
            &["work", "run", "--timeout", "1", "--", "sleep", "5"][..],
            &["work", "wait", "a", "b", "--any", "--tail", "2"],
            &["work", "wait", "--log", "/tmp/x", "done"],
            &["work", "wait", "--card", "c1", "--state", "review", "done"],
            &[
                "work", "add", "--title", "t", "--after", "a", "b", "--files", "/x", "--", "true",
            ],
            &[
                "work",
                "settings",
                "--cpu-budget",
                "4",
                "--self-accept",
                "true",
            ],
            &["work", "schedule", "add", "--every", "5m", "--", "true"],
            &["work", "update", "t1", "--assign", "ab"],
            &["pool", "lease", "--path", "/x"],
            &["quiet", "open", "--for-secs", "60"],
            &[
                "policy",
                "set",
                "core",
                "main",
                "--wip-limit",
                "3",
                "--reclaim",
            ],
            &["card", "link", "c1", "blocks", "c2"],
            &[
                "card",
                "create",
                "c1",
                "t",
                "--priority",
                "5",
                "--after",
                "c0",
            ],
            &["metrics", "--team", "core"],
            &["_supervise", "--job-dir", "/tmp/j"],
        ] {
            Cli::try_parse_from(std::iter::once("agora").chain(args.iter().copied()))
                .unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
    }

    #[test]
    fn schedule_add_needs_exactly_one_time() {
        assert!(Cli::try_parse_from(["agora", "work", "schedule", "add", "--", "true"]).is_err());
        assert!(
            Cli::try_parse_from([
                "agora", "work", "schedule", "add", "--at", "1", "--every", "60", "--", "true"
            ])
            .is_err()
        );
    }

    #[test]
    fn hook_invocation_skips_global_options() {
        for args in [
            vec!["agora", "hook", "context"],
            vec!["agora", "--json", "hook", "stop"],
            vec!["agora", "--home", "/x", "hook", "stop"],
            vec!["agora", "--home=/x", "hook"],
        ] {
            assert!(is_hook_invocation(&os(&args)), "{args:?}");
        }
    }

    #[test]
    fn other_invocations_are_not_hooks() {
        for args in [
            vec!["agora", "inbox"],
            vec!["agora", "chat", "hook"],
            vec!["agora"],
        ] {
            assert!(!is_hook_invocation(&os(&args)), "{args:?}");
        }
    }

    #[test]
    fn hook_parse_errors_are_silent_except_help_and_version() {
        let missing = Cli::try_parse_from(["agora", "hook"]).expect_err("no subcommand");
        assert!(hook_parse_error_is_silent(
            missing.kind(),
            &os(&["agora", "hook"])
        ));
        let help = Cli::try_parse_from(["agora", "hook", "--help"]).expect_err("help");
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(!hook_parse_error_is_silent(
            help.kind(),
            &os(&["agora", "hook", "--help"])
        ));
        let bad = Cli::try_parse_from(["agora", "inbox", "--bogus"]).expect_err("bad flag");
        assert!(!hook_parse_error_is_silent(
            bad.kind(),
            &os(&["agora", "inbox", "--bogus"])
        ));
    }

    #[test]
    fn hook_rewake_quick_parses() {
        let cli = parse(&strings(&["hook", "rewake", "--quick"]));
        let Cmd::Hook(HookArgs {
            cmd:
                HookCmd::Rewake {
                    quick,
                    max_wait_secs,
                },
        }) = cli.cmd
        else {
            panic!("not a rewake hook");
        };
        assert!(quick);
        assert_eq!(max_wait_secs, None);
    }

    #[test]
    fn hook_poll_parses_session_and_folder() {
        let cli = parse(&strings(&[
            "hook",
            "poll",
            "--session",
            "s",
            "--folder",
            "/tmp",
        ]));
        let Cmd::Hook(HookArgs {
            cmd: HookCmd::Poll { session, folder },
        }) = cli.cmd
        else {
            panic!("not a poll hook");
        };
        assert_eq!(session, "s");
        assert_eq!(folder, PathBuf::from("/tmp"));
    }

    #[test]
    fn install_parses_scope_mode_and_flags() {
        let cli = parse(&strings(&[
            "install",
            "--client",
            "claude",
            "--scope",
            "project",
            "--mode",
            "entries",
            "--probe",
            "--dry-run",
        ]));
        let Cmd::Install(a) = cli.cmd else {
            panic!("not install");
        };
        assert_eq!(a.client, "claude");
        assert_eq!(a.scope, ScopeArg::Project);
        assert_eq!(a.mode, ModeArg::Entries);
        assert!(a.probe);
        assert!(a.dry_run);
    }

    #[test]
    fn uninstall_defaults_to_user_scope_and_plugin_mode() {
        let cli = parse(&strings(&["uninstall", "--client", "claude"]));
        let Cmd::Uninstall(a) = cli.cmd else {
            panic!("not uninstall");
        };
        assert_eq!(a.scope, ScopeArg::User);
        assert_eq!(a.mode, ModeArg::Plugin);
        assert!(!a.probe);
        assert!(!a.dry_run);
    }

    #[test]
    fn unexpanded_placeholder_home_is_treated_as_unset() {
        let cli = parse(&strings(&["--home", "${AGORA_HOME:-}", "whoami"]));
        assert_eq!(home_of(&cli), DaemonConfig::default_home());
        let cli = parse(&strings(&["--home", "/x/home", "whoami"]));
        assert_eq!(home_of(&cli), PathBuf::from("/x/home"));
    }
}
