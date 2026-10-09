//! The agora MCP approval the bridge sends with `thread/start` and `thread/resume`.

use super::{CodexOptions, SAFE_AGORA_TOOLS, with_agora_tool_approval};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Agora tools that run commands, or change roles, work, channels, cards, pools or the files in a
/// chat. None of them may be pre-approved.
const DESTRUCTIVE: &[&str] = &[
    "work_run",
    "work_add",
    "work_accept",
    "work_claim",
    "work_schedule",
    "work_finish",
    "work_cancel",
    "role_grant",
    "revoke",
    "channel_create",
    "channel_add",
    "channel_remove",
    "channel_rotate",
    "channel_promote",
    "channel_demote",
    "card_move",
    "card_evidence",
    "card_approve",
    "card_reclaim",
    "attach",
    "fetch_file",
    "pool_define",
    "pool_lease",
    "pool_release",
    "quiet_window",
    "policy_set",
];

fn options(approve: bool) -> CodexOptions {
    let mut opts = CodexOptions::new(PathBuf::from("/unused/codex"), PathBuf::from("/tmp"));
    opts.approve_agora_tools = approve;
    opts
}

/// The tools `params` pre-approves under `mcp_servers.agora`.
fn approved_tools(params: &Value) -> BTreeSet<String> {
    params["config"]["mcp_servers"]["agora"]["tools"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, tool)| tool["approval_mode"] == "approve")
        .map(|(name, _)| name.clone())
        .collect()
}

#[test]
fn approves_exactly_the_read_only_allowlist_and_prompts_for_the_rest() {
    let params = with_agora_tool_approval(json!({}), &options(true));
    let expected: BTreeSet<String> = SAFE_AGORA_TOOLS
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    assert_eq!(approved_tools(&params), expected, "{params}");
    let mode = &params["config"]["mcp_servers"]["agora"]["default_tools_approval_mode"];
    assert_eq!(mode, "prompt", "{params}");
}

#[test]
fn destructive_agora_tools_are_never_approved() {
    let params = with_agora_tool_approval(json!({}), &options(true));
    let approved = approved_tools(&params);
    for tool in DESTRUCTIVE {
        assert!(!approved.contains(*tool), "{tool} must stay on prompt");
    }
}

#[test]
fn nothing_is_approved_when_the_flag_is_off() {
    let params = with_agora_tool_approval(json!({"cwd": "/tmp"}), &options(false));
    assert_eq!(params, json!({"cwd": "/tmp"}));
    assert!(approved_tools(&params).is_empty());
}
