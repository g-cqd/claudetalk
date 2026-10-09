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
