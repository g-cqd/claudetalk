//! Arguments of `agora daemon` and `agora mcp`, including the `mcp token` subcommands.

use clap::{Args, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;

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
