//! Arguments of `agora hook`: the Claude Code hook entry points and their dialect options.

use clap::{Args, Subcommand};
use std::path::PathBuf;

/// Options of `agora hook`.
#[derive(Args, Debug)]
pub struct HookArgs {
    #[command(subcommand)]
    pub cmd: HookCmd,
    /// Whose native hook protocol to speak: claude (default), codex, copilot, copilot-vscode, agy,
    /// cursor. An unknown value runs nothing and exits 0.
    #[arg(long, global = true)]
    pub dialect: Option<String>,
    /// Native event name when the payload does not carry it (at most 64 letters/underscores).
    #[arg(long, global = true)]
    pub event: Option<String>,
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
