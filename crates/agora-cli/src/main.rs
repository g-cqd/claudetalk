//! `agora`: command-line client and daemon launcher.
//!
//! Every command except `daemon` talks to the daemon over `$AGORA_HOME/agora.sock`,
//! auto-spawning it when absent. The CLI holds the private key (derived per folder
//! from the machine seed), signs every op locally and wraps/unwraps channel keys
//! locally: the daemon never sees a private key or a channel key.

mod cmd_board;
mod cmd_chat;
mod cmd_mcp;
mod cmd_org;

pub use agora_client::ClientError as CliError;
pub use agora_client::{Ctx, Out};

use agora_client::identity;
use agora_daemon::{DaemonConfig, Paths};
use clap::{Args, Parser, Subcommand};
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

#[derive(Subcommand, Debug)]
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

fn home_of(cli: &Cli) -> PathBuf {
    match &cli.home {
        Some(h) if !h.as_os_str().is_empty() => h.clone(),
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
        Cmd::Daemon(_) | Cmd::Mcp(_) => Err(CliError::Usage(
            "daemon and mcp are handled before dispatch".into(),
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
    }
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

fn main() -> ExitCode {
    let cli = Cli::parse();
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
}
