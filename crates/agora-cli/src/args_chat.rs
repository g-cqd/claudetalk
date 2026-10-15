//! Arguments of the chat commands: `chat`, `read`, `inbox`, `ack`, `react` and `fetch`.

use clap::Args;
use std::path::PathBuf;

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

/// `wait`: block until unacked messages arrive, without a timeout.
#[derive(Args, Debug)]
pub struct WaitArgs {
    /// Only messages with seq greater than this (cursor from a previous call).
    #[arg(long, default_value_t = 0)]
    pub after: i64,
    #[arg(long, default_value_t = 20)]
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
