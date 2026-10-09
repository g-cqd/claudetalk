//! Unit tests of the work handlers' argument mapping, refusals and the given-tasks plan.

use super::build::*;
use super::*;
use agora_client::Out;
use agora_sched::model::{RunArgs, ScopeArgs, ScopeKind, TaskRecord, TaskState, WaitResult};
use agora_sched::order::OrderPolicy;
use serde_json::json;
use std::path::Path;

mod args;
mod plan;
mod tail;
mod transport;
mod wait;

/// The error of a refused call: its code is always "usage"; returns its message.
fn refusal<T: std::fmt::Debug>(result: Result<T, ToolError>) -> String {
    let err = result.expect_err("the call is refused");
    assert_eq!(err.code, "usage");
    err.message
}

/// A `work_wait` call with no condition and the default timeout.
fn wait_args() -> WorkWaitArgs {
    serde_json::from_value(json!({})).expect("all fields are optional")
}
