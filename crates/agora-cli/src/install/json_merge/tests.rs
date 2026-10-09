use super::*;
use serde_json::json;
use std::path::PathBuf;

mod flat_named;
mod grouped;

/// A hook command as the installer writes it (`'<exe>' --home '<home>' hook <kind> --dialect <d>`).
fn ours_cmd(exe: &str, kind: &str, dialect: &str) -> String {
    format!("'{exe}' --home '/h' hook {kind} --dialect {dialect}")
}

fn ours(exe: &str, kind: &str, dialect: &str) -> Value {
    json!({"type": "command", "command": ours_cmd(exe, kind, dialect), "timeout": 10})
}

fn user() -> Value {
    json!({"type": "command", "command": "echo user-hook", "timeout": 5})
}

fn server(exe: &str) -> Value {
    json!({"type": "stdio", "command": exe, "args": ["--home", "/h", "mcp"]})
}

// --- is_our_command ---

#[test]
fn is_our_command_accepts_installer_commands() {
    assert!(is_our_command(&ours_cmd(
        "/usr/bin/agora",
        "context",
        "codex"
    )));
    assert!(is_our_command(&ours_cmd(
        "/usr/bin/agora",
        "session-start",
        "codex"
    )));
    assert!(is_our_command(
        "  agora --home /h hook stop --dialect cursor  "
    ));
}

#[test]
fn is_our_command_rejects_foreign_and_partial_commands() {
    assert!(!is_our_command("python3 hook.py"));
    assert!(!is_our_command("agora hook context"), "no --dialect");
    assert!(!is_our_command("agora --dialect codex"), "no hook");
    assert!(
        !is_our_command("other-tool hook x --dialect y"),
        "no --home"
    );
    assert!(
        !is_our_command("agora --home /h hook rewake --dialect codex"),
        "unknown hook kind"
    );
}

#[test]
fn is_our_command_rejects_a_hook_without_home() {
    assert!(!is_our_command("'/bin/agora' hook context --dialect codex"));
    assert!(!is_our_command("agora hook stop --dialect cursor"));
}

// --- check_object_member ---

#[test]
fn check_object_member_accepts_absent_and_object_members() {
    let path = PathBuf::from("/x/mcp.json");
    assert!(check_object_member(&json!({}), "mcpServers", &path).is_ok());
    assert!(check_object_member(&json!({"mcpServers": {}}), "mcpServers", &path).is_ok());
}

#[test]
fn check_object_member_names_path_and_key_when_not_an_object() {
    let path = PathBuf::from("/x/mcp.json");
    let err = check_object_member(&json!({"mcpServers": []}), "mcpServers", &path)
        .expect_err("array member must be rejected");
    let CliError::Usage(msg) = err else {
        panic!("expected a usage error, got {err:?}");
    };
    assert!(msg.contains("/x/mcp.json"), "{msg}");
    assert!(msg.contains("mcpServers"), "{msg}");
}

// --- check_event_arrays ---

#[test]
fn check_event_arrays_accepts_absent_and_array_events() {
    let path = PathBuf::from("/x/hooks.json");
    let events = ["SessionStart", "Stop", "PreToolUse"];
    assert!(check_event_arrays(&json!({}), &events, &path).is_ok());
    assert!(check_event_arrays(&json!({"hooks": {}}), &events, &path).is_ok());
    let root = json!({"hooks": {"Stop": [], "SessionStart": [{"hooks": [user()]}]}});
    assert!(check_event_arrays(&root, &events, &path).is_ok());
}

#[test]
fn check_event_arrays_names_path_and_event_when_not_an_array() {
    let path = PathBuf::from("/x/hooks.json");
    let root = json!({"hooks": {"Stop": {"hooks": []}}});
    let err = check_event_arrays(&root, &["SessionStart", "Stop"], &path)
        .expect_err("object event value must be rejected");
    let CliError::Usage(msg) = err else {
        panic!("expected a usage error, got {err:?}");
    };
    assert!(msg.contains("/x/hooks.json"), "{msg}");
    assert!(msg.contains("hooks.Stop"), "{msg}");
    assert!(msg.contains("not an array"), "{msg}");
    assert!(msg.contains("nothing was written"), "{msg}");
}

#[test]
fn check_event_arrays_rejects_a_null_event_value() {
    let path = PathBuf::from("/x/hooks.json");
    let root = json!({"hooks": {"stop": null}});
    assert!(check_event_arrays(&root, &["stop"], &path).is_err());
}

#[test]
fn check_event_arrays_leaves_a_non_object_hooks_member_to_check_object_member() {
    let path = PathBuf::from("/x/hooks.json");
    assert!(check_event_arrays(&json!({"hooks": []}), &["Stop"], &path).is_ok());
}

// --- set_server / remove_server ---

#[test]
fn set_server_adds_absent_entry_creating_the_container() {
    let mut root = json!({});
    assert_eq!(
        set_server(&mut root, "mcpServers", "agora", server("/bin/agora")),
        ServerChange::Added
    );
    assert_eq!(root, json!({"mcpServers": {"agora": server("/bin/agora")}}));
}

#[test]
fn set_server_equal_entry_is_unchanged_and_untouched() {
    let mut root = json!({"mcpServers": {"agora": server("/bin/agora")}});
    let before = root.clone();
    assert_eq!(
        set_server(&mut root, "mcpServers", "agora", server("/bin/agora")),
        ServerChange::Unchanged
    );
    assert_eq!(root, before);
}

#[test]
fn set_server_replaces_stale_exe_in_place_and_keeps_siblings() {
    let sibling = json!({"command": "node", "args": ["srv.js"]});
    let mut root = json!({"mcpServers": {"agora": server("/old/agora"), "other": sibling.clone()}});
    assert_eq!(
        set_server(&mut root, "mcpServers", "agora", server("/new/agora")),
        ServerChange::Updated
    );
    assert_eq!(
        root,
        json!({"mcpServers": {"agora": server("/new/agora"), "other": sibling}})
    );
}

#[test]
fn set_server_replaces_foreign_entry_and_reports_it() {
    let mut root = json!({"mcpServers": {"agora": {"command": "npx", "args": ["agora-x"]}}});
    assert_eq!(
        set_server(&mut root, "mcpServers", "agora", server("/bin/agora")),
        ServerChange::ReplacedForeign
    );
    assert_eq!(root, json!({"mcpServers": {"agora": server("/bin/agora")}}));
}

#[test]
fn set_server_handles_opencode_command_arrays() {
    let ours_arr = json!({"type": "local", "command": ["/old/agora", "--home", "/h", "mcp"]});
    let mut root = json!({"mcp": {"agora": ours_arr}});
    let new = json!({"type": "local", "command": ["/new/agora", "--home", "/h", "mcp"]});
    assert_eq!(
        set_server(&mut root, "mcp", "agora", new.clone()),
        ServerChange::Updated
    );
    assert_eq!(root, json!({"mcp": {"agora": new}}));

    let mut root = json!({"mcp": {"agora": {"type": "local", "command": ["npx", "x"]}}});
    assert_eq!(
        set_server(&mut root, "mcp", "agora", new.clone()),
        ServerChange::ReplacedForeign
    );
    assert_eq!(root, json!({"mcp": {"agora": new}}));
}

#[test]
fn set_server_leaves_non_object_container_untouched() {
    let mut root = json!({"mcpServers": "not-an-object"});
    let before = root.clone();
    assert_eq!(
        set_server(&mut root, "mcpServers", "agora", server("/bin/agora")),
        ServerChange::ReplacedForeign
    );
    assert_eq!(root, before);
}

#[test]
fn set_server_twice_equals_once() {
    let mut root = json!({});
    set_server(&mut root, "mcpServers", "agora", server("/bin/agora"));
    let once = root.clone();
    assert_eq!(
        set_server(&mut root, "mcpServers", "agora", server("/bin/agora")),
        ServerChange::Unchanged
    );
    assert_eq!(root, once);
}

#[test]
fn remove_server_drops_the_container_it_empties() {
    let mut root = json!({"mcpServers": {"agora": server("/bin/agora")}});
    assert!(remove_server(&mut root, "mcpServers", "agora"));
    assert_eq!(root, json!({}));
}

#[test]
fn remove_server_keeps_user_entries() {
    let other = json!({"command": "node"});
    let mut root = json!({"mcpServers": {"agora": server("/bin/agora"), "other": other.clone()}});
    assert!(remove_server(&mut root, "mcpServers", "agora"));
    assert_eq!(root, json!({"mcpServers": {"other": other}}));
}

#[test]
fn remove_server_ignores_foreign_and_absent_entries() {
    let foreign = json!({"command": "npx"});
    let mut root = json!({"mcpServers": {"agora": foreign}});
    let before = root.clone();
    assert!(!remove_server(&mut root, "mcpServers", "agora"));
    assert_eq!(root, before);
    assert!(!remove_server(&mut root, "mcpServers", "missing"));
    assert!(!remove_server(&mut json!({}), "mcpServers", "agora"));
}

// --- server ownership (is_agora_exe) ---

#[test]
fn is_agora_exe_matches_the_file_name_not_the_directory() {
    assert!(is_agora_exe("/opt/agora/bin/agora"));
    assert!(is_agora_exe("  /usr/local/bin/agora  "));
    assert!(!is_agora_exe("/Users/agora/bin/my-mcp"), "dir named agora");
    assert!(!is_agora_exe("npx"));
}

#[test]
fn is_agora_exe_accepts_a_quoted_path_with_spaces() {
    assert!(is_agora_exe("'/Users/me/My Apps/agora'"));
    assert!(!is_agora_exe("'/Users/me/My Apps/my-mcp'"));
}

#[test]
fn user_server_whose_directory_mentions_agora_is_not_ours() {
    let user_entry = json!({"command": "/Users/agora/bin/my-mcp", "args": ["serve"]});
    let mut root = json!({"mcpServers": {"agora": user_entry.clone()}});
    assert!(!remove_server(&mut root, "mcpServers", "agora"));
    assert_eq!(root, json!({"mcpServers": {"agora": user_entry}}));
    assert_eq!(
        set_server(&mut root, "mcpServers", "agora", server("/bin/agora")),
        ServerChange::ReplacedForeign
    );
}

#[test]
fn agora_executable_in_any_directory_is_ours() {
    let mut root = json!({"mcpServers": {"agora": server("/opt/agora/bin/agora")}});
    assert_eq!(
        set_server(&mut root, "mcpServers", "agora", server("/new/agora")),
        ServerChange::Updated
    );
    assert!(remove_server(&mut root, "mcpServers", "agora"));
    assert_eq!(root, json!({}));
}

#[test]
fn quoted_agora_exe_with_spaces_is_ours() {
    let quoted = json!({"type": "stdio", "command": "'/Users/me/My Apps/agora'", "args": ["mcp"]});
    let mut root = json!({"mcpServers": {"agora": quoted}});
    assert_eq!(
        set_server(&mut root, "mcpServers", "agora", server("/bin/agora")),
        ServerChange::Updated
    );
    assert_eq!(root, json!({"mcpServers": {"agora": server("/bin/agora")}}));
}
