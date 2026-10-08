//! The error every client operation returns (the CLI's former `CliError`).

use agora_crypto::CryptoError;
use agora_proto::{ErrorBody, ProtoError};
use agora_store::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{0}")]
    Remote(ErrorBody),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    Usage(String),
    #[error("daemon unavailable: {0}")]
    Daemon(String),
    #[error("unexpected reply from daemon")]
    UnexpectedReply,
}

impl ClientError {
    /// Stable machine code for --json: the daemon's ErrorCode (snake_case) for Remote,
    /// else "io" | "proto" | "crypto" | "store" | "usage" | "daemon" | "unexpected_reply".
    pub fn code(&self) -> String {
        match self {
            ClientError::Remote(b) => serde_json::to_value(b.code)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| "internal".into()),
            ClientError::Io(_) => "io".into(),
            ClientError::Proto(_) => "proto".into(),
            ClientError::Crypto(_) => "crypto".into(),
            ClientError::Store(_) => "store".into(),
            ClientError::Usage(_) => "usage".into(),
            ClientError::Daemon(_) => "daemon".into(),
            ClientError::UnexpectedReply => "unexpected_reply".into(),
        }
    }
}
