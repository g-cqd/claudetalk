//! Per-Claude-session hook state. The implementation moved to `agora_client::session_state`
//! so that the MCP push task and the hook rungs share one cursor per session; this module
//! re-exports it so the hook call sites keep their `crate::state::*` paths.

pub use agora_client::session_state::*;
