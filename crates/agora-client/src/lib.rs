//! Shared client code of the `agora` CLI and `agora-mcp`: the socket client, the
//! per-folder identity, the signing/crypto glue and one function per verb (`api`), so
//! both front-ends run the same implementation and print the same JSON.

pub mod api;
pub mod capabilities;
pub mod client;
pub mod error;
pub mod heartbeat;
pub mod identity;
pub mod ops;

pub use error::ClientError;

use agora_crypto::Identity;
use agora_daemon::Paths;

/// What every verb needs.
pub struct Ctx {
    pub paths: Paths,
    /// This folder's identity.
    pub identity: Identity,
    /// Folder name, used as the handshake/registration label.
    pub label: String,
}

/// A verb's result: `json` is printed with --json, `text` otherwise.
#[derive(Debug, Clone)]
pub struct Out {
    pub json: serde_json::Value,
    pub text: String,
}
