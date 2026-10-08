//! The agora daemon: sole owner of the database, event bus, and unix-socket server.
//!
//! Concurrency model (see SPEC): one writer thread owns the read-write `Store`
//! (commands arrive on a bounded mpsc queue and are answered on oneshots); one
//! reader thread owns a second `Store` connection on the same WAL database for
//! read-only requests, so long reads never queue behind writes and no SQLite
//! connection is ever shared between threads. The writer publishes [`Event`]s on a
//! `tokio::sync::broadcast` bus after each successful commit, in commit order.

pub mod actor;
pub mod admission;
pub mod auth;
pub mod awaiter;
pub mod background;
pub mod board;
pub mod chat;
pub mod clock;
pub mod config;
pub mod dispatch;
pub mod jobs;
pub mod org;
pub mod presence;
pub mod push;
pub mod reconciler;
pub mod sched;
pub mod server;
pub mod system;
pub mod timers;
pub mod wait;
pub mod watch;
pub mod work;

pub use config::{DaemonConfig, Paths};
pub use server::{Daemon, Injected, run_blocking};

use agora_proto::{Event, ProtoError};
use agora_store::{Pub, StoreError};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Capacity of the event bus; a waiter that falls further behind gets `Lagged`
/// and simply re-checks.
pub const BUS_CAPACITY: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("another daemon is running (lock held: {0})")]
    AlreadyRunning(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error("store actor stopped")]
    ActorGone,
    #[error("{0}")]
    Config(String),
}

/// State shared by every connection task (cheap to clone).
#[derive(Clone)]
pub struct Shared {
    pub writer: actor::Writer,
    pub reader: actor::Reader,
    pub bus: broadcast::Sender<Event>,
    /// Identities that currently hold a subscribed connection.
    pub presence: presence::Presence,
    pub config: Arc<DaemonConfig>,
    /// Effective uid of the daemon process.
    pub daemon_uid: u32,
    /// Cancelled once when the daemon begins shutting down.
    pub shutdown: CancellationToken,
    /// Time source of all M5 code (`SystemClock`, or a `FakeClock` injected by tests).
    pub clock: Arc<dyn clock::Clock>,
    /// Machine probe (`RealMachine`, or a `FakeMachine` injected by tests).
    pub machine: Arc<dyn agora_run::machine::Machine>,
    /// The daemon's own signing identity; registered in the store as the system identity.
    pub system: Arc<agora_crypto::Identity>,
    /// Wakes the admission loop: notify after anything that can change who may start.
    pub kick: Arc<tokio::sync::Notify>,
    /// Live jobs, pending environments and held-since bookkeeping.
    pub jobs: jobs::JobTable,
}

/// One authenticated connection.
#[derive(Debug, Clone)]
pub struct Conn {
    /// Key proven in the handshake; the reader/actor identity of every request.
    pub pubkey: Pub,
    /// Peer uid from SO_PEERCRED / getpeereid.
    pub uid: u32,
    /// ClientHello.label truncated to 64 bytes (at a char boundary).
    pub label: String,
}

/// Wall clock in milliseconds since the Unix epoch (the store's `now`).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
