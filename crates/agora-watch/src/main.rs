//! `agora-watch`: a read-only watcher web app for a running Agora daemon.
//!
//! Serves one embedded HTML page plus a JSON API that mirrors the read verbs of
//! `agora_client::api` (the same implementation the `agora` CLI uses), so every
//! aspect of the daemon state — identity, channels, messages, board, notes, work
//! queue, scheduler, delivery and files — can be inspected live in a browser.
//! No endpoint writes or signs anything: the watcher only reads.

mod api;
mod files;

use agora_client::Ctx;
use agora_client::identity;
use agora_daemon::{DaemonConfig, Paths};
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

/// Default bind address: loopback only, the watcher is a local inspector.
const DEFAULT_ADDR: &str = "127.0.0.1:7360";

#[derive(Parser, Debug)]
#[command(
    name = "agora-watch",
    version,
    about = "Read-only watcher web app for Agora"
)]
struct Cli {
    /// Data directory (default: $AGORA_HOME, else ~/.agora).
    #[arg(long, global = true, env = "AGORA_HOME")]
    home: Option<PathBuf>,
    /// The folder whose identity the watcher acts as (default: the data directory).
    #[arg(long, global = true, env = "AGORA_FOLDER")]
    folder: Option<PathBuf>,
    /// Address to serve on.
    #[arg(long, default_value = DEFAULT_ADDR)]
    addr: String,
}

/// The data directory: `--home`/`AGORA_HOME` when set, else the daemon default.
fn home_of(cli: &Cli) -> PathBuf {
    match &cli.home {
        Some(h) if !h.as_os_str().is_empty() && !h.to_string_lossy().contains("${") => h.clone(),
        _ => DaemonConfig::default_home(),
    }
}

fn main() {
    let cli = Cli::parse();
    let home = home_of(&cli);
    let paths = Paths::new(&home);
    let folder = cli.folder.unwrap_or_else(|| home.clone());
    // The watcher never creates state: the machine seed must already exist.
    let (identity, label) = match identity::load_existing_identity(&paths, &folder) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("agora-watch: {e}");
            std::process::exit(1);
        }
    };
    let ctx = Arc::new(Ctx {
        paths,
        identity,
        label,
    });
    let addr: SocketAddr = match cli.addr.parse() {
        Ok(a) => a,
        Err(_) => {
            eprintln!("agora-watch: invalid address {}", cli.addr);
            std::process::exit(1);
        }
    };
    let app = api::router(ctx);
    println!(
        "agora-watch: http://{addr}  (home {}, identity folder {})",
        home.display(),
        folder.display()
    );
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(async {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("agora-watch: cannot bind {addr}: {e}");
                    std::process::exit(1);
                });
            axum::serve(listener, app).await.expect("server");
        });
}
