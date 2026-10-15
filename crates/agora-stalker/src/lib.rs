//! `agora-stalker`: an all-seeing, read-only watcher web app for a running
//! Agora daemon (BETA).
//!
//! Serves one embedded HTML page plus a JSON API that mirrors the read verbs of
//! `agora_client::api` (the same implementation the `agora` CLI uses): every
//! channel and its messages with threads grouped, every status, the board,
//! notes, the work queue, the scheduler, delivery and files — live in a
//! browser. No endpoint writes or signs anything: the stalker only reads.
//!
//! With a supervisor-or-above identity it uses the oversight reads
//! (`all_channels`, `all_messages`, `all_statuses`) and shows everything from
//! everyone, including channels the identity is not a member of. With a worker
//! identity it falls back to that identity's own view and says so. Two limits
//! are honest and stay: end-to-end-encrypted channel bodies stay opaque
//! unless the stalker's identity holds the channel key (the daemon cannot
//! decrypt them for anyone), and a worker never sees beyond its own view.

pub mod api;
mod files;

use agora_client::Ctx;
use std::net::SocketAddr;
use std::sync::Arc;

/// Default bind address: loopback only, the stalker is a local inspector.
pub const DEFAULT_ADDR: &str = "127.0.0.1:7360";

/// Bind `addr` and serve the embedded page plus the JSON API as `ctx` until the
/// process ends. The bind error names the address, like the standalone binary.
pub async fn serve(ctx: Arc<Ctx>, addr: SocketAddr) -> std::io::Result<()> {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("agora-stalker: cannot bind {addr}: {e}");
            return Err(e);
        }
    };
    axum::serve(listener, api::router(ctx)).await
}
