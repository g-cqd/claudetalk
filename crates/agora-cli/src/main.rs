//! `agora`: command-line client and daemon launcher.
//!
//! Every command except `daemon` talks to the daemon over `$AGORA_HOME/agora.sock`,
//! auto-spawning it when absent. The CLI holds the private key (derived per folder
//! from the machine seed), signs every op locally and wraps/unwraps channel keys
//! locally: the daemon never sees a private key or a channel key.

mod client;
mod cmd_board;
mod cmd_chat;
mod cmd_org;
mod identity;
mod ops;

use agora_crypto::{CryptoError, Identity};
use agora_daemon::{DaemonConfig, Paths};
use agora_proto::{ErrorBody, ProtoError};
use agora_store::StoreError;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
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
    Daemon,
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

/// What every command needs.
pub struct Ctx {
    pub paths: Paths,
    /// This folder's identity.
    pub identity: Identity,
    /// Folder name, used as the handshake/registration label.
    pub label: String,
    pub json: bool,
}

/// A command's result: `json` is printed with --json, `text` otherwise.
pub struct Out {
    pub json: serde_json::Value,
    pub text: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Remote(ErrorBody),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    Usage(String),
    #[error("daemon unavailable: {0}")]
    Daemon(String),
    #[error("unexpected reply from daemon")]
    UnexpectedReply,
}

impl CliError {
    /// Stable machine code for --json: the daemon's ErrorCode (snake_case) for Remote,
    /// else "io" | "proto" | "crypto" | "store" | "usage" | "daemon" | "unexpected_reply".
    pub fn code(&self) -> String {
        match self {
            CliError::Remote(b) => serde_json::to_value(b.code)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| "internal".into()),
            CliError::Io(_) => "io".into(),
            CliError::Proto(_) => "proto".into(),
            CliError::Crypto(_) => "crypto".into(),
            CliError::Store(_) => "store".into(),
            CliError::Usage(_) => "usage".into(),
            CliError::Daemon(_) => "daemon".into(),
            CliError::UnexpectedReply => "unexpected_reply".into(),
        }
    }
}

fn home_of(cli: &Cli) -> PathBuf {
    match &cli.home {
        Some(h) if !h.as_os_str().is_empty() => h.clone(),
        _ => DaemonConfig::default_home(),
    }
}

fn run_daemon(home: PathBuf) -> Result<(), String> {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AGORA_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    let config = DaemonConfig::load(&home).map_err(|e| e.to_string())?;
    agora_daemon::run_blocking(config).map_err(|e| e.to_string())
}

async fn dispatch(ctx: &Ctx, cmd: Cmd) -> Result<Out, CliError> {
    match cmd {
        Cmd::Daemon => Err(CliError::Usage("daemon is handled before dispatch".into())),
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let home = home_of(&cli);
    let json = cli.json;
    if matches!(cli.cmd, Cmd::Daemon) {
        return match run_daemon(home) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("agora: daemon: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let result = (|| -> Result<Out, CliError> {
        let paths = Paths::new(&home);
        let (identity, label) = identity::load_identity(&paths)?;
        let ctx = Ctx {
            paths,
            identity,
            label,
            json,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(dispatch(&ctx, cli.cmd))
    })();
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
