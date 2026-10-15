//! Tool handlers: status_set, status_clear, status_list.

use super::args::*;
use super::{ToolCx, ToolResult, framed_output, output};
use agora_client::api::status::{self as api, StatusClearParams, StatusSetParams};
use agora_deliver::Harness;

/// `api::status::status_set`, with `harness` filled from the peer's client
/// record when the caller does not name one, so a status names the harness
/// that wrote it even when the model omits the field.
pub async fn status_set(cx: &ToolCx<'_>, a: StatusSetArgs) -> ToolResult {
    let harness = a.harness.or_else(|| Some(harness_str(cx)));
    let out = api::status_set(
        &cx.peer.ctx,
        StatusSetParams {
            state: a.state,
            harness,
            folder: a.folder,
            matter: a.matter,
        },
    )
    .await?;
    Ok(output(out))
}

/// `api::status::status_clear`; `harness` defaults to the peer's harness.
pub async fn status_clear(cx: &ToolCx<'_>, a: StatusClearArgs) -> ToolResult {
    let harness = a.harness.or_else(|| Some(harness_str(cx)));
    let out = api::status_clear(&cx.peer.ctx, StatusClearParams { harness }).await?;
    Ok(output(out))
}

/// `api::status::statuses`. The text and the JSON carry other agents' text
/// (state, folder, matter), so both are framed as untrusted data.
pub async fn status_list(cx: &ToolCx<'_>, _a: StatusListArgs) -> ToolResult {
    let out = api::statuses(&cx.peer.ctx).await?;
    Ok(framed_output(out))
}

/// The harness name of this peer, as `whoami` reports it.
fn harness_str(cx: &ToolCx<'_>) -> String {
    match cx.engine.peer_harness(cx.peer) {
        Harness::ClaudeCode => "claude",
        Harness::Codex => "codex",
        Harness::CopilotCli => "copilot",
        Harness::CopilotVsCode => "copilot-vscode",
        Harness::OpenCode => "opencode",
        Harness::Cursor => "cursor",
        Harness::Antigravity => "agy",
        Harness::Unknown => "unknown",
    }
    .to_string()
}
