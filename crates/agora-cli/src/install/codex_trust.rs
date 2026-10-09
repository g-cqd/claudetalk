//! Codex hook trust (SPEC 2.1, VERIFIED-LOCAL against Codex 0.157.1): untrusted hooks are skipped
//! until `[hooks.state."<key>"] trusted_hash` is in the user config. Recipe verified against
//! `hooks/list` currentHash for 12 events: /tmp/agora-m6/fixtures/codex/hook_trust_hash.py.
//! `hooks/list` reports `timeoutSec: 3` for the `interrupt` and `sessionEnd` handlers (VERIFIED-LOCAL,
//! /tmp/agora-m6/probe-codex/phase0*.jsonl), so the hash clamps those two timeouts to 3 seconds.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Timeout ceiling Codex applies to the `session_end` and `interrupt` events.
const SHORT_EVENT_TIMEOUT_CAP_S: u64 = 3;
/// Timeout used for a handler entry that has no `timeout` field. DOCS, not VERIFIED-LOCAL:
/// learn.chatgpt.com/docs/hooks states 600 s; no local Codex 0.157.1 probe
/// (/tmp/agora-m6/probe-codex/phase0*.jsonl) covered a handler without an explicit timeout.
const DEFAULT_TIMEOUT_S: u64 = 600;

/// PascalCase hook event -> snake_case label (`SessionStart` -> `session_start`, `UserPromptSubmit` ->
/// `user_prompt_submit`, `PostToolUse` -> `post_tool_use`, `Stop` -> `stop`, ...). `None` if unknown.
pub fn event_label(event: &str) -> Option<&'static str> {
    Some(match event {
        "SessionStart" => "session_start",
        "UserPromptSubmit" => "user_prompt_submit",
        "PreToolUse" => "pre_tool_use",
        "PermissionRequest" => "permission_request",
        "PostToolUse" => "post_tool_use",
        "PreCompact" => "pre_compact",
        "PostCompact" => "post_compact",
        "SubagentStart" => "subagent_start",
        "SubagentStop" => "subagent_stop",
        "Stop" => "stop",
        "Interrupt" => "interrupt",
        "SessionEnd" => "session_end",
        _ => return None,
    })
}

/// `sha256:<hex>` of the compact JSON (sorted keys, separators `,` `:`, no ASCII escaping) of
/// `{"event_name":<label>,"hooks":[{"async":false,"command":C,"timeout":<secs>,"type":"command"}],"matcher":M}`
/// with `matcher` omitted when `None`. Codex records no matcher for `user_prompt_submit`, `stop` and
/// `interrupt`, and clamps the timeout of `session_end` and `interrupt` to 3 seconds, so both are
/// applied here before hashing.
pub fn trusted_hash(label: &str, matcher: Option<&str>, command: &str, timeout_s: u64) -> String {
    let matcher = match label {
        "user_prompt_submit" | "stop" | "interrupt" => None,
        _ => matcher,
    };
    let timeout_s = match label {
        "session_end" | "interrupt" => timeout_s.min(SHORT_EVENT_TIMEOUT_CAP_S),
        _ => timeout_s,
    };
    // Built by hand so the key order is fixed whatever serde_json's `preserve_order` feature is.
    // A JSON string from `Value::String(..).to_string()` is compact and leaves non-ASCII as-is,
    // like Python's `json.dumps(ensure_ascii=False)`.
    let quote = |s: &str| Value::String(s.to_owned()).to_string();
    let matcher_part = match matcher {
        Some(m) => format!(",\"matcher\":{}", quote(m)),
        None => String::new(),
    };
    let body = format!(
        "{{\"event_name\":{},\"hooks\":[{{\"async\":false,\"command\":{},\"timeout\":{timeout_s},\"type\":\"command\"}}]{matcher_part}}}",
        quote(label),
        quote(command),
    );
    let digest = Sha256::digest(body.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// `<canonicalized absolute path of hooks.json>:<label>:<group_index>:<handler_index>` (the caller
/// passes the already canonicalized path).
pub fn trust_key(hooks_json_canonical: &Path, label: &str, group: usize, handler: usize) -> String {
    format!(
        "{}:{}:{}:{}",
        hooks_json_canonical.display(),
        label,
        group,
        handler
    )
}

/// Walk `hooks_doc.hooks` (events, groups, handlers) and return `(trust_key, trusted_hash)` for each
/// handler whose `(event, command)` pair is in `expected`: the exact pairs this install run wrote, as
/// PascalCase event names and exact command strings. Nothing else is trusted: a handler that only
/// carries agora's magic substrings (`json_merge::is_our_command`) is not ours to trust.
///
/// The pair must also occur once in its event. Then that handler is the position this run wrote; a
/// second copy of the same command in the same event makes the position ambiguous, so no copy is
/// trusted. Hash inputs are the group's `matcher` and the handler's `command` / `timeout` (seconds).
///
/// The path is canonicalized when it exists; when canonicalization fails the given path is used as is.
/// Entries are sorted by key, so the result does not depend on the JSON object's key order.
pub fn our_trust_entries(
    hooks_json_path: &Path,
    hooks_doc: &Value,
    expected: &[(&str, &str)],
) -> Vec<(String, String)> {
    let canonical =
        std::fs::canonicalize(hooks_json_path).unwrap_or_else(|_| hooks_json_path.to_path_buf());
    let Some(events) = hooks_doc.get("hooks").and_then(Value::as_object) else {
        return Vec::new();
    };
    // (event, command, trust key, trusted hash) of every handler with a pair this run wrote.
    let mut found: Vec<(&str, &str, String, String)> = Vec::new();
    for (event, groups) in events {
        let (Some(label), Some(groups)) = (event_label(event), groups.as_array()) else {
            continue;
        };
        for (gi, group) in groups.iter().enumerate() {
            let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            let matcher = group.get("matcher").and_then(Value::as_str);
            for (hi, handler) in handlers.iter().enumerate() {
                let Some(command) = handler.get("command").and_then(Value::as_str) else {
                    continue;
                };
                if !expected.contains(&(event.as_str(), command)) {
                    continue;
                }
                let timeout = handler
                    .get("timeout")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_TIMEOUT_S);
                found.push((
                    event.as_str(),
                    command,
                    trust_key(&canonical, label, gi, hi),
                    trusted_hash(label, matcher, command, timeout),
                ));
            }
        }
    }
    let mut out: Vec<(String, String)> = found
        .iter()
        .filter(|(event, command, _, _)| {
            found
                .iter()
                .filter(|(e, c, _, _)| e == event && c == command)
                .count()
                == 1
        })
        .map(|(_, _, key, hash)| (key.clone(), hash.clone()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    const SESSION_START_CMD: &str = "python3 -I /tmp/agora-m6/probe-codex/hook.py SessionStart";
    const POST_TOOL_USE_CMD: &str = "python3 -I /tmp/agora-m6/probe-codex/hook.py PostToolUse";
    const INTERRUPT_CMD: &str = "python3 -I /tmp/agora-m6/probe-codex/hook.py Interrupt";

    #[test]
    fn hash_matches_the_codex_recipe_for_session_start() {
        assert_eq!(
            trusted_hash("session_start", None, SESSION_START_CMD, 10),
            "sha256:d85c796fb5f3ee55a2b90bb8f41b1c40904412dec370306c8932ddbb802e895b"
        );
    }

    #[test]
    fn hash_includes_the_matcher_when_given() {
        assert_eq!(
            trusted_hash("post_tool_use", Some("*"), POST_TOOL_USE_CMD, 10),
            "sha256:5d3049a51156171ded900f3240626caacd3f24c342dfc354dfcc80f107cdc860"
        );
    }

    #[test]
    fn hash_escapes_quotes_and_keeps_non_ascii_raw() {
        let cmd = r#"python3 -I /tmp/agora-m6/probe-codex/hook.py "é says \"hi\"""#;
        assert_eq!(
            trusted_hash("pre_tool_use", Some("Bash|Edit"), cmd, 5),
            "sha256:b68ad94cb90dce89869eaa76d89897f22b7d05febeba11287a63ed02c9dbeab5"
        );
    }

    #[test]
    fn stop_ignores_a_given_matcher_and_clamps_nothing() {
        assert_eq!(
            trusted_hash("stop", Some("*"), POST_TOOL_USE_CMD, 10),
            "sha256:9561775f65d9f4624f3caa7a6df177e747626412a979f62a6e0d6c868fb7a885"
        );
    }

    #[test]
    fn interrupt_timeout_is_clamped_to_three_seconds() {
        assert_eq!(
            trusted_hash("interrupt", None, INTERRUPT_CMD, 10),
            "sha256:343d971cfa2041dae9061a26d83434ce92ef169c7ba9c27f11009180a2871645"
        );
        assert_eq!(
            trusted_hash("session_end", None, INTERRUPT_CMD, 10),
            trusted_hash("session_end", None, INTERRUPT_CMD, 3)
        );
    }

    #[test]
    fn event_label_maps_the_twelve_known_events_only() {
        let pairs = [
            ("SessionStart", "session_start"),
            ("UserPromptSubmit", "user_prompt_submit"),
            ("PreToolUse", "pre_tool_use"),
            ("PermissionRequest", "permission_request"),
            ("PostToolUse", "post_tool_use"),
            ("PreCompact", "pre_compact"),
            ("PostCompact", "post_compact"),
            ("SubagentStart", "subagent_start"),
            ("SubagentStop", "subagent_stop"),
            ("Stop", "stop"),
            ("Interrupt", "interrupt"),
            ("SessionEnd", "session_end"),
        ];
        for (event, label) in pairs {
            assert_eq!(event_label(event), Some(label), "{event}");
        }
        assert_eq!(event_label("stop"), None);
        assert_eq!(event_label("Notification"), None);
    }

    #[test]
    fn trust_key_joins_path_label_and_indices() {
        assert_eq!(
            trust_key(Path::new("/home/u/.codex/hooks.json"), "stop", 2, 1),
            "/home/u/.codex/hooks.json:stop:2:1"
        );
    }

    /// The four `(event, command)` pairs this run wrote for the handlers of the test documents.
    fn written() -> Vec<(&'static str, &'static str)> {
        vec![
            (
                "SessionStart",
                "'/usr/bin/agora' --home '/h' hook session-start --dialect codex",
            ),
            (
                "PreToolUse",
                "'/usr/bin/agora' --home '/h' hook context --dialect codex",
            ),
            (
                "Stop",
                "'/usr/bin/agora' --home '/h' hook stop --dialect codex",
            ),
            (
                "PostToolUse",
                "'/usr/bin/agora' --home '/h' hook context --dialect codex",
            ),
        ]
    }

    const SPOOF: &str = "echo # --home x hook stop --dialect codex";

    #[test]
    fn our_trust_entries_skip_user_hooks_and_key_our_handlers_by_position() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hooks.json");
        fs::write(&path, "{}").expect("write hooks.json");
        let doc = json!({"hooks": {
            "SessionStart": [{"hooks": [
                {"type": "command", "command": "'/usr/bin/agora' --home '/h' hook session-start --dialect codex", "timeout": 10}
            ]}],
            "PreToolUse": [{"matcher": "Bash", "hooks": [
                {"type": "command", "command": "/usr/local/bin/my-lint --fix"},
                {"type": "command", "command": "'/usr/bin/agora' --home '/h' hook context --dialect codex", "timeout": 30}
            ]}],
            "Stop": [{"matcher": "x", "hooks": [
                {"type": "command", "command": "'/usr/bin/agora' --home '/h' hook stop --dialect codex"}
            ]}],
            "PostToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "echo hi"}]},
                {"hooks": [{"type": "command", "command": "'/usr/bin/agora' --home '/h' hook context --dialect codex"}]}
            ],
            "Notification": [{"hooks": [{"type": "command", "command": "'/usr/bin/agora' --home '/h' hook x --dialect codex"}]}]
        }});
        let canon = fs::canonicalize(&path).expect("canonicalize");
        let got = our_trust_entries(&path, &doc, &written());
        let prefix = canon.display();
        let keys: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                format!("{prefix}:post_tool_use:1:0"),
                format!("{prefix}:pre_tool_use:0:1"),
                format!("{prefix}:session_start:0:0"),
                format!("{prefix}:stop:0:0"),
            ]
        );
        let hashes: Vec<&str> = got.iter().map(|(_, h)| h.as_str()).collect();
        assert_eq!(
            hashes,
            [
                trusted_hash(
                    "post_tool_use",
                    None,
                    "'/usr/bin/agora' --home '/h' hook context --dialect codex",
                    600
                ),
                trusted_hash(
                    "pre_tool_use",
                    Some("Bash"),
                    "'/usr/bin/agora' --home '/h' hook context --dialect codex",
                    30
                ),
                trusted_hash(
                    "session_start",
                    None,
                    "'/usr/bin/agora' --home '/h' hook session-start --dialect codex",
                    10
                ),
                trusted_hash(
                    "stop",
                    None,
                    "'/usr/bin/agora' --home '/h' hook stop --dialect codex",
                    600
                ),
            ]
        );
    }

    #[test]
    fn our_trust_entries_use_the_given_path_when_it_cannot_be_canonicalized() {
        let path = Path::new("/nonexistent-agora-m6/hooks.json");
        let doc = json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "'/usr/bin/agora' --home '/h' hook stop --dialect codex"}
        ]}]}});
        let got = our_trust_entries(path, &doc, &[("Stop", STOP_CMD)]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "/nonexistent-agora-m6/hooks.json:stop:0:0");
    }

    const STOP_CMD: &str = "'/usr/bin/agora' --home '/h' hook stop --dialect codex";

    /// Writes `doc` to a temp hooks.json and returns the trust entries for `expected`.
    fn entries_for(doc: &Value, expected: &[(&str, &str)]) -> Vec<(String, String)> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hooks.json");
        fs::write(&path, "{}").expect("write hooks.json");
        our_trust_entries(&path, doc, expected)
    }

    #[test]
    fn spoofed_pre_tool_use_handler_with_magic_substrings_yields_no_trust() {
        let doc = json!({"hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": STOP_CMD}]}],
            "PreToolUse": [{"matcher": "*", "hooks": [
                {"type": "command", "command": "# --home x hook stop --dialect codex"}
            ]}]
        }});
        let got = entries_for(&doc, &[("Stop", STOP_CMD)]);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].0.ends_with(":stop:0:0"), "{got:?}");
    }

    #[test]
    fn spoofed_handler_in_a_group_with_ours_yields_no_trust_for_the_spoof() {
        let doc = json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": STOP_CMD},
            {"type": "command", "command": SPOOF}
        ]}]}});
        let got = entries_for(&doc, &[("Stop", STOP_CMD)]);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].0.ends_with(":stop:0:0"), "{got:?}");
        assert_eq!(
            got[0].1,
            trusted_hash("stop", None, STOP_CMD, DEFAULT_TIMEOUT_S)
        );
    }

    #[test]
    fn exact_command_under_another_event_is_not_trusted() {
        let doc = json!({"hooks": {"PostToolUse": [{"hooks": [
            {"type": "command", "command": STOP_CMD}
        ]}]}});
        assert!(entries_for(&doc, &[("Stop", STOP_CMD)]).is_empty());
    }

    #[test]
    fn near_miss_command_is_not_trusted() {
        let doc = json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": format!("{STOP_CMD} ")}
        ]}]}});
        assert!(entries_for(&doc, &[("Stop", STOP_CMD)]).is_empty());
    }

    #[test]
    fn a_pair_occurring_twice_in_its_event_yields_no_trust() {
        let doc = json!({"hooks": {"Stop": [
            {"hooks": [{"type": "command", "command": STOP_CMD}]},
            {"hooks": [{"type": "command", "command": STOP_CMD}]}
        ]}});
        assert!(entries_for(&doc, &[("Stop", STOP_CMD)]).is_empty());
    }
}
