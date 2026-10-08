//! Agora's MCP front-end: a hand-rolled JSON-RPC 2.0 layer (stdio and stateless HTTP)
//! exposing the `agora_client::api` verbs as MCP tools.
//!
//! Layers: `jsonrpc` (wire), `version` (protocol eras), `engine` (method routing),
//! `tools` (the tool table and handlers), `framing` (the injection defence for inbound
//! text), `push` + `delivery` (Claude Code channel push and escalation bookkeeping),
//! `clients` (who is calling), `stdio` / `http` (transports), `tokens` (HTTP bearer
//! tokens). Logs go to stderr only: stdout of `agora mcp` is the protocol stream.

pub mod clients;
pub mod delivery;
pub mod engine;
pub mod framing;
pub mod http;
pub mod instructions;
pub mod jsonrpc;
pub mod push;
pub mod stdio;
pub mod tokens;
pub mod tools;
pub mod version;

use agora_client::{ClientError, Ctx};
use engine::EngineConfig;

/// The `serverInfo.name` this server reports.
pub const SERVER_NAME: &str = "agora";
/// The `serverInfo.version` this server reports (the package version).
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Client(#[from] ClientError),
    /// Bad configuration (for example a non-loopback bind without --allow-remote).
    #[error("{0}")]
    Config(String),
    /// Token file problem (unsafe mode, symlink, bad JSON, bad label, ...).
    #[error("{0}")]
    Token(String),
}

/// Serve MCP over this process's stdin/stdout until EOF or a shutdown signal.
///
/// Builds `Engine::new(cfg)` and `Peer::new(Arc::new(ctx), Transport::Stdio, None)`, then runs
/// `stdio::serve(engine, peer, tokio::io::stdin(), tokio::io::stdout(), token)` with a shutdown
/// token that SIGINT/SIGTERM cancel. Identity is the folder key in `ctx`, signed locally.
#[allow(unused_variables)]
pub async fn run_stdio(ctx: Ctx, cfg: EngineConfig) -> Result<(), McpError> {
    let engine = engine::Engine::new(cfg);
    let peer = std::sync::Arc::new(engine::Peer::new(
        std::sync::Arc::new(ctx),
        engine::Transport::Stdio,
        None,
    ));
    let token = tokio_util::sync::CancellationToken::new();
    let signal_token = token.clone();
    let signals = tokio::spawn(async move {
        #[cfg(unix)]
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut term) => {
                    term.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();
        let ctrl_c = async {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            _ = ctrl_c => {}
            _ = terminate => {}
        }
        signal_token.cancel();
    });
    let result = stdio::serve(engine, peer, tokio::io::stdin(), tokio::io::stdout(), token).await;
    signals.abort();
    result
}
