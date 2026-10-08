//! Everything that touches processes and the machine: job directories, the supervisor, process probes, QoS
//! wrapping, program lookup and the machine probe. The API is synchronous except `spawn::spawn_supervisor`.

pub mod jobdir;
pub mod locate;
pub mod machine;
pub mod proc;
pub mod qos;
pub mod spawn;
pub mod supervise;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(String),
}
