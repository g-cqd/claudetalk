//! `agora-stalker`: an all-seeing, read-only watcher web app for a running
//! Agora daemon (BETA). The same server is hosted by the `agora` CLI as
//! `agora stalker` (alias `agora watch`).

use agora_client::Ctx;
use agora_client::identity;
use agora_daemon::{DaemonConfig, Paths};
use agora_stalker::DEFAULT_ADDR;
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "agora-stalker",
    version,
    about = "All-seeing read-only watcher web app for Agora (BETA)"
)]
struct Cli {
    /// Data directory (default: $AGORA_HOME, else ~/.agora).
    #[arg(long, global = true, env = "AGORA_HOME")]
    home: Option<PathBuf>,
    /// The folder whose identity the stalker acts as (default: $AGORA_FOLDER,
    /// else the current folder; the same identity rule as every agora command).
    /// Give it a supervisor-or-above identity to see everything from everyone.
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
    // The stalker never creates state: the machine seed must already exist.
    let folder = match &cli.folder {
        Some(f) => std::fs::canonicalize(f).unwrap_or_else(|e| {
            eprintln!("agora-stalker: {f:?}: {e}");
            std::process::exit(1);
        }),
        // The current folder (AGORA_FOLDER or the cwd), so the stalker's
        // identity is the one whose rights it inherits.
        None => match identity::current_folder() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("agora-stalker: {e}");
                std::process::exit(1);
            }
        },
    };
    let (identity, label) = match identity::load_existing_identity(&paths, &folder) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("agora-stalker: {e}");
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
            eprintln!("agora-stalker: invalid address {}", cli.addr);
            std::process::exit(1);
        }
    };
    println!(
        "agora-stalker: http://{addr}  (home {}, identity folder {})",
        home.display(),
        folder.display()
    );
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(async {
            if let Err(e) = agora_stalker::serve(ctx, addr).await {
                eprintln!("agora-stalker: {e}");
                std::process::exit(1);
            }
        });
}
