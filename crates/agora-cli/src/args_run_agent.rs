//! Arguments of `agora run-agent`: the harness it bridges and that harness's options.

use clap::{Args, Subcommand};
use std::path::PathBuf;

/// Options of `agora run-agent`.
#[derive(Args, Debug)]
pub struct RunAgentArgs {
    #[command(subcommand)]
    pub cmd: RunAgentCmd,
}

/// Which harness `agora run-agent` bridges. `--cwd` defaults to the current directory
/// (canonicalized) and is also the identity folder.
#[derive(Subcommand, Debug)]
pub enum RunAgentCmd {
    /// Codex via `codex app-server`.
    Codex {
        /// Session root and identity folder (default: the current directory).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Resume this thread instead of starting one.
        #[arg(long)]
        thread: Option<String>,
        /// First prompt to send once the thread is ready.
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value = "workspace-write",
            value_parser = ["read-only", "workspace-write", "danger-full-access"])]
        sandbox: String,
        #[arg(long, default_value = "never",
            value_parser = ["never", "on-request", "untrusted"])]
        approval: String,
        /// Do not pre-approve the agora MCP tools. By default the bridge approves only read-only
        /// agora chat and board tools, never commands.
        #[arg(long)]
        no_approve_agora_tools: bool,
        /// The codex binary (default: `codex` on PATH).
        #[arg(long)]
        bin: Option<PathBuf>,
    },
    /// Copilot CLI via ACP (`copilot --acp --stdio`).
    Copilot {
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Load this ACP session instead of creating one.
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        /// Auto-approve permission requests ONLY for read-only agora chat/board tools; everything
        /// else is rejected.
        #[arg(long, alias = "allow-tools")]
        allow_agora_tools: bool,
        /// The copilot binary (default: `copilot` on PATH).
        #[arg(long)]
        bin: Option<PathBuf>,
    },
    /// opencode via its HTTP server (`opencode serve`, loopback only).
    Opencode {
        /// Server URL (default http://127.0.0.1:4096).
        #[arg(long)]
        url: Option<String>,
        /// Use this session instead of creating one.
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Name of the env var holding the server password (never the password itself).
        #[arg(long)]
        password_env: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
    },
    /// Antigravity via `agy -p --conversation`.
    Agy {
        /// The conversation to deliver into.
        #[arg(long)]
        conversation: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// The agy binary (default: `agy` on PATH).
        #[arg(long)]
        bin: Option<PathBuf>,
        /// Per-delivery timeout in seconds. A run that reaches it is killed and its batch counts
        /// as delivered.
        #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..))]
        timeout_secs: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser, Debug)]
    struct Cli {
        #[command(subcommand)]
        cmd: RunAgentCmd,
    }

    fn copilot_allows_agora_tools(argv: &[&str]) -> bool {
        match Cli::try_parse_from(argv).expect("parses").cmd {
            RunAgentCmd::Copilot {
                allow_agora_tools, ..
            } => allow_agora_tools,
            other => panic!("not copilot: {other:?}"),
        }
    }

    #[test]
    fn allow_agora_tools_and_its_hidden_alias_set_the_same_flag() {
        assert!(copilot_allows_agora_tools(&[
            "x",
            "copilot",
            "--allow-agora-tools"
        ]));
        assert!(copilot_allows_agora_tools(&[
            "x",
            "copilot",
            "--allow-tools"
        ]));
        assert!(!copilot_allows_agora_tools(&["x", "copilot"]));
    }

    #[test]
    fn the_old_allow_tools_flag_is_not_in_the_help() {
        let help = Cli::command()
            .find_subcommand_mut("copilot")
            .expect("copilot subcommand")
            .render_help()
            .to_string();
        assert!(help.contains("--allow-agora-tools"), "{help}");
        assert!(!help.contains("--allow-tools"), "{help}");
    }

    #[test]
    fn agy_timeout_defaults_to_600_seconds_and_rejects_zero() {
        match Cli::try_parse_from(["x", "agy", "--conversation", "c1"])
            .expect("parses")
            .cmd
        {
            RunAgentCmd::Agy { timeout_secs, .. } => assert_eq!(timeout_secs, 600),
            other => panic!("not agy: {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["x", "agy", "--conversation", "c1", "--timeout-secs", "0"])
                .is_err(),
            "a zero timeout would count every run as handed over"
        );
    }
}
