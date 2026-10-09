use super::*;

// --- flat hooks (Cursor shape) ---

#[test]
fn merge_flat_adds_version_and_handlers_to_an_empty_file() {
    let mut root = json!({});
    let h = ours("/bin/agora", "session-start", "cursor");
    assert_eq!(
        merge_flat_hooks(&mut root, &[("sessionStart", h.clone())]),
        1
    );
    assert_eq!(root, json!({"version": 1, "hooks": {"sessionStart": [h]}}));
}

#[test]
fn merge_flat_keeps_an_existing_version() {
    let mut root = json!({"version": 2, "hooks": {"stop": [user()]}});
    merge_flat_hooks(&mut root, &[("stop", ours("/bin/agora", "stop", "cursor"))]);
    assert_eq!(root["version"], json!(2));
    assert_eq!(root["hooks"]["stop"][0], user());
}

#[test]
fn merge_flat_is_idempotent_and_replaces_stale_exe_in_place() {
    let new = ours("/new/agora", "stop", "cursor");
    let mut root =
        json!({"version": 1, "hooks": {"stop": [user(), ours("/old/agora", "stop", "cursor")]}});
    let entries = [("stop", new.clone())];
    assert_eq!(merge_flat_hooks(&mut root, &entries), 1);
    assert_eq!(
        root,
        json!({"version": 1, "hooks": {"stop": [user(), new]}})
    );
    let once = root.clone();
    assert_eq!(merge_flat_hooks(&mut root, &entries), 0);
    assert_eq!(root, once);
}

#[test]
fn remove_flat_leaves_an_empty_object_for_a_file_we_created() {
    let mut root = json!({});
    merge_flat_hooks(
        &mut root,
        &[
            (
                "sessionStart",
                ours("/bin/agora", "session-start", "cursor"),
            ),
            ("stop", ours("/bin/agora", "stop", "cursor")),
        ],
    );
    assert_eq!(remove_flat_hooks(&mut root), 2);
    assert_eq!(root, json!({}));
}

#[test]
fn remove_flat_keeps_version_and_user_handlers_when_content_remains() {
    let mut root = json!({"version": 1, "hooks": {"stop": [user()]}});
    merge_flat_hooks(&mut root, &[("stop", ours("/bin/agora", "stop", "cursor"))]);
    assert_eq!(remove_flat_hooks(&mut root), 1);
    assert_eq!(root, json!({"version": 1, "hooks": {"stop": [user()]}}));
}

#[test]
fn remove_flat_prunes_only_the_events_we_emptied() {
    let mut root = json!({"version": 1, "hooks": {
        "a": [ours("/bin/agora", "context", "cursor")],
        "b": [user()],
        "c": [],
    }});
    assert_eq!(remove_flat_hooks(&mut root), 1);
    assert_eq!(
        root,
        json!({"version": 1, "hooks": {"b": [user()], "c": []}})
    );
}

#[test]
fn remove_flat_is_a_no_op_without_ours() {
    let mut root = json!({"version": 1, "hooks": {"stop": [user()]}});
    let before = root.clone();
    assert_eq!(remove_flat_hooks(&mut root), 0);
    assert_eq!(root, before);
}

// --- named hooks (agy shape) ---

fn agy_def(exe: &str) -> Value {
    json!({
        "enabled": true,
        "PreInvocation": [{"type": "command", "command": ours_cmd(exe, "context", "agy"), "timeout": 10}],
        "Stop": [{"type": "command", "command": ours_cmd(exe, "stop", "agy"), "timeout": 10}],
    })
}

#[test]
fn set_named_covers_added_unchanged_updated_and_foreign() {
    let mut root = json!({});
    assert_eq!(
        set_named(&mut root, "agora", agy_def("/bin/agora")),
        ServerChange::Added
    );
    assert_eq!(
        set_named(&mut root, "agora", agy_def("/bin/agora")),
        ServerChange::Unchanged
    );
    assert_eq!(
        set_named(&mut root, "agora", agy_def("/new/agora")),
        ServerChange::Updated
    );
    assert_eq!(root, json!({"agora": agy_def("/new/agora")}));

    let mut root = json!({"agora": {"enabled": false, "Stop": [user()]}});
    assert_eq!(
        set_named(&mut root, "agora", agy_def("/bin/agora")),
        ServerChange::ReplacedForeign
    );
    assert_eq!(root, json!({"agora": agy_def("/bin/agora")}));
}

#[test]
fn remove_named_removes_ours_and_keeps_other_keys() {
    let other = json!({"x": 1});
    let mut root = json!({"agora": agy_def("/bin/agora"), "other": other.clone()});
    assert!(remove_named(&mut root, "agora"));
    assert_eq!(root, json!({"other": other}));
}

#[test]
fn remove_named_keeps_foreign_values_and_leaves_empty_object_for_our_file() {
    let foreign = json!({"x": 1});
    let mut root = json!({"agora": foreign.clone()});
    assert!(!remove_named(&mut root, "agora"));
    assert_eq!(root, json!({"agora": foreign}));

    let mut root = json!({});
    set_named(&mut root, "agora", agy_def("/bin/agora"));
    assert!(remove_named(&mut root, "agora"));
    assert_eq!(root, json!({}));
}
