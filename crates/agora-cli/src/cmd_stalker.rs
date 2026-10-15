//! `agora stalker` (BETA): serve the all-seeing read-only watcher web app.
//!
//! The stalker acts with the current folder's identity (the same identity rule
//! as every other agora command). With a supervisor-or-above identity it shows
//! everything from everyone through the oversight reads; with a worker
//! identity it falls back to that identity's own view and says so on the page.
//! Encrypted channel bodies stay opaque unless the identity holds the key.

use crate::StalkerArgs;
use agora_client::Ctx;
use agora_client::identity;
use agora_daemon::Paths;
use std::sync::Arc;

pub fn run(home: std::path::PathBuf, a: StalkerArgs) -> Result<(), String> {
    let paths = Paths::new(&home);
    let (identity, label) = identity::load_identity(&paths).map_err(|e| e.to_string())?;
    let ctx = Arc::new(Ctx {
        paths,
        identity,
        label,
    });
    let addr = a.addr;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    println!(
        "agora stalker: http://{addr}  (home {}, identity folder {})",
        home.display(),
        agora_client::identity::current_folder()
            .map(|f| f.display().to_string())
            .unwrap_or_default()
    );
    rt.block_on(async {
        agora_stalker::serve(ctx, addr)
            .await
            .map_err(|e| e.to_string())
    })
}
