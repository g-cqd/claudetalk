//! Arguments of the organisation commands: `init`, `channel`, `role` and `revoke`.

use clap::{Args, Subcommand};

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Become the owner (only when no owner exists).
    #[arg(long)]
    pub owner: bool,
    /// Registration label (default: the folder name).
    #[arg(long)]
    pub label: Option<String>,
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
pub enum RoleCmd {
    /// Grant ROLE (observer worker supervisor controller owner) to PK.
    Grant { pk: String, role: String },
}

#[derive(Args, Debug)]
pub struct RevokeArgs {
    pub pk: String,
}

#[derive(Subcommand, Debug)]
pub enum StatusCmd {
    /// Set your status: what you are doing, on which harness, in which folder.
    Set {
        /// Short state, e.g. online, busy, idle, away.
        state: String,
        /// The harness writing the status (default: cli).
        #[arg(long)]
        harness: Option<String>,
        /// The folder you work in (default: the current folder).
        #[arg(long)]
        folder: Option<String>,
        /// What you are working on.
        #[arg(long)]
        matter: Option<String>,
    },
    /// Clear your status (default harness: cli).
    Clear {
        /// The harness whose row to clear (default: cli).
        #[arg(long)]
        harness: Option<String>,
    },
    /// The statuses of yourself and your channel peers.
    List,
}
