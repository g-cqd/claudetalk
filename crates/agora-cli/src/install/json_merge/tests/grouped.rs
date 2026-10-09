use super::*;

// --- grouped hooks (Codex / Claude shape) ---

#[test]
fn merge_grouped_adds_groups_with_optional_matcher() {
    let (h1, h2) = (
        ours("/bin/agora", "session-start", "codex"),
        ours("/bin/agora", "context", "codex"),
    );
    let entries = [
        ("SessionStart", None, h1.clone()),
        ("PreToolUse", Some("Bash"), h2.clone()),
    ];
    let mut root = json!({});
    assert_eq!(merge_grouped_hooks(&mut root, &entries), 2);
    assert_eq!(
        root,
        json!({"hooks": {
            "SessionStart": [{"hooks": [h1]}],
            "PreToolUse": [{"matcher": "Bash", "hooks": [h2]}]
        }})
    );
}

#[test]
fn merge_grouped_is_idempotent() {
    let entries = [("Stop", None, ours("/bin/agora", "stop", "codex"))];
    let mut root = json!({"hooks": {"Stop": [{"hooks": [user()]}]}});
    assert_eq!(merge_grouped_hooks(&mut root, &entries), 1);
    let once = root.clone();
    assert_eq!(merge_grouped_hooks(&mut root, &entries), 0);
    assert_eq!(root, once);
}

#[test]
fn merge_grouped_replaces_stale_exe_in_place_and_keeps_user_handlers() {
    let new = ours("/new/agora", "stop", "codex");
    let mut root = json!({"hooks": {"Stop": [
        {"matcher": "Bash", "hooks": [user()]},
        {"hooks": [user(), ours("/old/agora", "stop", "codex")]}
    ]}});
    assert_eq!(
        merge_grouped_hooks(&mut root, &[("Stop", None, new.clone())]),
        1
    );
    assert_eq!(
        root,
        json!({"hooks": {"Stop": [
            {"matcher": "Bash", "hooks": [user()]},
            {"hooks": [user(), new]}
        ]}})
    );
}

#[test]
fn merge_grouped_never_modifies_user_groups() {
    let user_group = json!({"matcher": "Edit", "hooks": [user()]});
    let user_event = json!([{"hooks": [user()]}]);
    let mut root =
        json!({"hooks": {"Notification": user_event.clone(), "PreToolUse": [user_group.clone()]}});
    merge_grouped_hooks(
        &mut root,
        &[("PreToolUse", None, ours("/bin/agora", "context", "codex"))],
    );
    assert_eq!(root["hooks"]["Notification"], user_event);
    assert_eq!(root["hooks"]["PreToolUse"][0], user_group);
    assert_eq!(
        root["hooks"]["PreToolUse"].as_array().map(Vec::len),
        Some(2)
    );
}

#[test]
fn merge_grouped_keeps_two_of_our_handlers_in_one_event() {
    let a = ours("/bin/agora", "session-start", "codex");
    let b = ours("/bin/agora", "context", "codex");
    let entries = [("E", None, a.clone()), ("E", None, b.clone())];
    let mut root = json!({});
    assert_eq!(merge_grouped_hooks(&mut root, &entries), 2);
    assert_eq!(merge_grouped_hooks(&mut root, &entries), 0);
    assert_eq!(
        root,
        json!({"hooks": {"E": [{"hooks": [a]}, {"hooks": [b]}]}})
    );
}

#[test]
fn merge_grouped_keeps_a_hook_without_home_and_appends_ours() {
    let old = json!({"type": "command", "command": "agora hook stop --dialect codex"});
    let new = ours("/bin/agora", "stop", "codex");
    let mut root = json!({"hooks": {"Stop": [{"hooks": [old.clone()]}]}});
    assert_eq!(
        merge_grouped_hooks(&mut root, &[("Stop", None, new.clone())]),
        1
    );
    assert_eq!(
        root,
        json!({"hooks": {"Stop": [{"hooks": [old]}, {"hooks": [new]}]}})
    );
}

#[test]
fn merge_grouped_ignores_entries_without_a_command() {
    let mut root = json!({});
    assert_eq!(
        merge_grouped_hooks(&mut root, &[("Stop", None, json!({"timeout": 1}))]),
        0
    );
    assert_eq!(root, json!({}));
}

#[test]
fn remove_grouped_removes_only_ours_and_keeps_user_parts() {
    let user_group = json!({"matcher": "Edit", "hooks": [user()]});
    let mut root = json!({"hooks": {
        "PreToolUse": [
            user_group.clone(),
            {"hooks": [ours("/bin/agora", "context", "codex")]},
        ],
        "Stop": [{"hooks": [user(), ours("/bin/agora", "stop", "codex")]}],
        "Notification": [],
    }});
    assert_eq!(remove_grouped_hooks(&mut root), 2);
    assert_eq!(
        root,
        json!({"hooks": {
            "PreToolUse": [user_group],
            "Stop": [{"hooks": [user()]}],
            "Notification": [],
        }})
    );
}

#[test]
fn remove_grouped_leaves_an_empty_object_for_a_file_we_created() {
    let mut root = json!({});
    merge_grouped_hooks(
        &mut root,
        &[
            (
                "SessionStart",
                None,
                ours("/bin/agora", "session-start", "codex"),
            ),
            (
                "PreToolUse",
                Some("Bash"),
                ours("/bin/agora", "context", "codex"),
            ),
        ],
    );
    assert_eq!(remove_grouped_hooks(&mut root), 2);
    assert_eq!(root, json!({}));
}

#[test]
fn remove_grouped_is_a_no_op_without_ours() {
    let mut root = json!({"hooks": {"Stop": [{"hooks": [user()]}]}});
    let before = root.clone();
    assert_eq!(remove_grouped_hooks(&mut root), 0);
    assert_eq!(root, before);
    assert_eq!(remove_grouped_hooks(&mut json!({})), 0);
}
