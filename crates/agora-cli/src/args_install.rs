//! Arguments of `agora install` and `agora uninstall`, with the client scope and mode they take.

use clap::Args;

/// Options of `agora install` / `agora uninstall`.
#[derive(Args, Debug)]
pub struct InstallArgs {
    /// Agent client(s) to configure: one of claude, codex, copilot-cli, copilot-vscode, agy,
    /// opencode, cursor; a comma-separated list of them; or `all`.
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
    /// Codex only: also write the hook trust entries into the user config.toml, so Codex runs the
    /// agora hooks without the interactive `/hooks` review.
    #[arg(long)]
    pub trust_hooks: bool,
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
