//! `agora mcp` (stdio / HTTP) and `agora mcp token ...`.

use crate::{CliError, McpArgs, Out, TokenCmd};
use agora_client::{Ctx, identity};
use agora_daemon::{Paths, now_ms};
use agora_mcp::engine::{Engine, EngineConfig};
use agora_mcp::http::{self, HttpOptions};
use agora_mcp::tokens::{self, TokenEntry};
use serde_json::json;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::signal::unix::SignalKind;
use tokio_util::sync::CancellationToken;

/// Run the MCP server until its input closes or it is told to stop.
///
/// Without `--http`: stdio. Identity is this folder's key (`identity::load_identity`),
/// exactly like the CLI; the protocol stream is stdout, so nothing else may print there.
/// With `--http ADDR`: the HTTP front-end until ctrl-c or SIGTERM. Own tracing init to STDERR
/// (filter env AGORA_LOG, default "warn") and its own multi-thread tokio runtime. Errors are
/// returned as the message printed after `agora: mcp: `.
pub fn run(home: PathBuf, a: McpArgs) -> Result<(), String> {
    if a.http.is_none() && a.token_file.is_some() {
        return Err("--token-file needs --http".into());
    }
    init_tracing();
    let paths = Paths::new(&home);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let result = match a.http {
        None => identity::load_identity(&paths)
            .map_err(|e| e.to_string())
            .and_then(|(identity, label)| {
                let ctx = Ctx {
                    paths,
                    identity,
                    label,
                };
                rt.block_on(agora_mcp::run_stdio(ctx, EngineConfig::stdio()))
                    .map_err(|e| e.to_string())
            }),
        Some(addr) => rt.block_on(serve_http(paths, addr, a)),
    };
    // A stdin read blocked in a worker thread cannot be cancelled, so do not wait for it at exit.
    rt.shutdown_timeout(Duration::from_millis(200));
    result
}

/// The standalone HTTP front-end: bind, check the token file, serve until a shutdown signal.
async fn serve_http(paths: Paths, addr: SocketAddr, a: McpArgs) -> Result<(), String> {
    let listener = http::bind(addr, a.allow_remote)
        .await
        .map_err(|e| e.to_string())?;
    let opts = http_options(&paths, a.token_file, a.allow_origin);
    check_tokens(&opts.token_file)?;
    let local = listener.local_addr().map_err(|e| e.to_string())?;
    let shutdown = CancellationToken::new();
    let signals = spawn_signal_cancel(shutdown.clone());
    eprintln!("agora mcp: listening on http://{local}/mcp");
    let engine = Engine::new(EngineConfig::http());
    let result = http::serve(listener, opts, engine, shutdown)
        .await
        .map_err(|e| e.to_string());
    signals.abort();
    result
}

/// Cancels `token` on the first ctrl-c or SIGTERM. A signal stream that cannot be registered
/// simply never fires, so the other one still works.
fn spawn_signal_cancel(token: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let terminate = async {
            match tokio::signal::unix::signal(SignalKind::terminate()) {
                Ok(mut term) => {
                    term.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        let ctrl_c = async {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            _ = ctrl_c => {}
            _ = terminate => {}
        }
        token.cancel();
    })
}

/// `agora mcp token add|list|revoke` on the token file: `token_file`, else `tokens::default_path`.
///
/// `add --label L`: `tokens::add`; text shows the token (once), id and the identity's public
/// key; json `{"id","label","token","pubkey","pseudonym"}`. `list`: json `{"tokens":[{"id",
/// "label","created","pubkey","pseudonym"}]}`, never the token, its hash or the seed. `revoke
/// ID`: json `{"id","revoked","pubkey"}` (the revoked entry's public key, null when nothing was
/// revoked); text names that key and points at `agora revoke <pk>` to remove the identity too.
/// An unknown id is `revoked: false`, not an error. Token errors map to `CliError::Usage`. The
/// output mode (`json`) is applied by the caller.
pub fn token(
    home: PathBuf,
    _json: bool,
    token_file: Option<PathBuf>,
    c: TokenCmd,
) -> Result<Out, CliError> {
    let path = token_file.unwrap_or_else(|| tokens::default_path(&Paths::new(&home)));
    match c {
        TokenCmd::Add { label } => {
            let (token, entry) = tokens::add(&path, &label, now_ms()).map_err(usage)?;
            let (pubkey, pseudonym) = public_of(&entry)?;
            Ok(Out {
                json: json!({
                    "id": entry.id,
                    "label": entry.label,
                    "token": token,
                    "pubkey": pubkey,
                    "pseudonym": pseudonym,
                }),
                text: format!(
                    "token: {token}\nid: {}  pubkey: {pubkey}\nThe token is shown once; it lets \
                     its holder act as this identity (server-held key).",
                    entry.id
                ),
            })
        }
        TokenCmd::List => {
            let file = tokens::load(&path).map_err(usage)?;
            let mut rows = Vec::with_capacity(file.tokens.len());
            let mut lines = Vec::with_capacity(file.tokens.len());
            for entry in &file.tokens {
                let (pubkey, pseudonym) = public_of(entry)?;
                lines.push(format!(
                    "{}  {pubkey}  {pseudonym}  {}",
                    entry.id, entry.label
                ));
                rows.push(json!({
                    "id": entry.id,
                    "label": entry.label,
                    "created": entry.created,
                    "pubkey": pubkey,
                    "pseudonym": pseudonym,
                }));
            }
            let text = if lines.is_empty() {
                "no tokens".to_string()
            } else {
                lines.join("\n")
            };
            Ok(Out {
                json: json!({ "tokens": rows }),
                text,
            })
        }
        TokenCmd::Revoke { id } => {
            // The entry's public key is what `agora revoke` takes, so read it before revoking.
            // An entry whose identity cannot be derived is still revoked, just without a key.
            let file = tokens::load(&path).map_err(usage)?;
            let before = file
                .tokens
                .iter()
                .find(|entry| entry.id == id)
                .and_then(|entry| public_of(entry).ok())
                .map(|(pubkey, _)| pubkey);
            let revoked = tokens::revoke(&path, &id).map_err(usage)?;
            let (text, pubkey) = match (revoked, before) {
                (true, Some(pubkey)) => (
                    format!(
                        "revoked {id}\npubkey: {pubkey}\nrun `agora revoke {pubkey}` to also \
                         remove the identity"
                    ),
                    Some(pubkey),
                ),
                (true, None) => (format!("revoked {id}"), None),
                (false, _) => (format!("no token {id}"), None),
            };
            Ok(Out {
                json: json!({ "id": id, "revoked": revoked, "pubkey": pubkey }),
                text,
            })
        }
    }
}

/// Options of the HTTP front-end: the token file defaults to `tokens::default_path`.
pub fn http_options(
    paths: &Paths,
    token_file: Option<PathBuf>,
    allowed_origins: Vec<String>,
) -> HttpOptions {
    HttpOptions {
        paths: paths.clone(),
        token_file: token_file.unwrap_or_else(|| tokens::default_path(paths)),
        allowed_origins,
    }
}

/// Refuses a token file that cannot be used (symlink, group/other access, bad JSON) before the
/// server listens, and warns on stderr when it holds no tokens, since every request would
/// then get 401.
pub fn check_tokens(path: &Path) -> Result<(), String> {
    let file = tokens::load(path).map_err(|e| e.to_string())?;
    if file.tokens.is_empty() {
        eprintln!(
            "agora mcp: no tokens in {}; create one with `agora mcp token add --label NAME`",
            path.display()
        );
    }
    Ok(())
}

/// Tracing to stderr only: stdout is the protocol stream of stdio.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AGORA_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    // try_init: a subscriber installed earlier in this process is kept as it is.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn usage(e: agora_mcp::McpError) -> CliError {
    CliError::Usage(e.to_string())
}

/// The lowercase-hex public key and the pseudonym of a token's identity.
fn public_of(entry: &TokenEntry) -> Result<(String, String), CliError> {
    let identity = tokens::identity_of(entry).map_err(usage)?;
    let pubkey: String = identity
        .public()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((pubkey, identity.pseudonym()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    fn add(home: &Path, label: &str) -> Out {
        token(
            home.to_path_buf(),
            false,
            None,
            TokenCmd::Add {
                label: label.to_string(),
            },
        )
        .expect("add")
    }

    fn list(home: &Path) -> Out {
        token(home.to_path_buf(), false, None, TokenCmd::List).expect("list")
    }

    #[test]
    fn token_add_list_revoke_round_trip() {
        let dir = home_dir();
        let home = dir.path();
        let added = add(home, "laptop");
        let secret = added.json["token"].as_str().expect("token").to_string();
        let id = added.json["id"].as_str().expect("id").to_string();
        assert!(secret.starts_with("agora_"));
        assert!(added.text.contains(&secret));
        let pubkey = added.json["pubkey"].as_str().expect("pubkey");
        assert_eq!(pubkey.len(), 64);
        assert!(pubkey.bytes().all(|b| b.is_ascii_hexdigit()));

        let listed = list(home);
        let rows = listed.json["tokens"].as_array().expect("tokens array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"].as_str(), Some(id.as_str()));
        assert_eq!(rows[0]["label"].as_str(), Some("laptop"));
        assert_eq!(rows[0]["pubkey"].as_str(), Some(pubkey));
        // Listing exposes no secret: neither the token nor the stored seed appears in it.
        let file = tokens::load(&tokens::default_path(&Paths::new(home))).expect("load");
        let seed = file.tokens[0].seed.as_str();
        let shown = format!("{} {}", listed.json, listed.text);
        assert!(!shown.contains(&secret));
        assert!(!shown.contains(seed));
        assert!(!shown.contains(&file.tokens[0].token_sha256));

        let revoked = token(
            home.to_path_buf(),
            false,
            None,
            TokenCmd::Revoke { id: id.clone() },
        )
        .expect("revoke");
        assert_eq!(revoked.json["revoked"], json!(true));
        assert_eq!(revoked.json["pubkey"].as_str(), Some(pubkey));
        assert_eq!(
            revoked.text,
            format!(
                "revoked {id}\npubkey: {pubkey}\nrun `agora revoke {pubkey}` to also remove the \
                 identity"
            )
        );
        assert!(
            list(home).json["tokens"]
                .as_array()
                .expect("array")
                .is_empty()
        );
    }

    #[test]
    fn revoking_an_unknown_id_reports_not_revoked() {
        let dir = home_dir();
        let out = token(
            dir.path().to_path_buf(),
            false,
            None,
            TokenCmd::Revoke {
                id: "t_00000000".to_string(),
            },
        )
        .expect("revoke");
        assert_eq!(out.json["revoked"], json!(false));
        assert!(out.json["pubkey"].is_null());
        assert_eq!(out.text, "no token t_00000000");
    }

    #[test]
    fn token_file_option_replaces_the_default_file() {
        let dir = home_dir();
        let custom = dir.path().join("elsewhere.json");
        token(
            dir.path().to_path_buf(),
            false,
            Some(custom.clone()),
            TokenCmd::Add {
                label: "ci".to_string(),
            },
        )
        .expect("add");
        assert!(custom.exists());
        assert!(!tokens::default_path(&Paths::new(dir.path())).exists());
    }
}
