//! Pure scheduling logic and the shared data model of the work queue (a port of the g-cqd work.py planner).
//!
//! No I/O, no tokio, no filesystem and no clock reads: `now_ms` is always a parameter. Time inside the admission
//! math is f64 seconds (to mirror work.py); everything stored or sent over the wire is i64 milliseconds.

pub mod admit;
pub mod assign;
pub mod claims;
pub mod compat;
pub mod cron;
pub mod envscrub;
pub mod exit;
pub mod graph;
pub mod model;
pub mod model_ops;
pub mod model_reports;
pub mod order;
pub mod plan;
pub mod policy;
pub mod predict;
pub mod pyfmt;
pub mod quiet;
pub mod reconcile;
pub mod render;
pub mod settings;
pub mod signature;
pub mod task;
pub mod textfmt;
pub mod timer;
