use super::*;

/// A realistic user config: comments, a model, a trusted project, a server the user runs by hand
/// (with an env subtable) and a trust entry the user wrote for their own hooks file.
const USER: &str = r#"# Codex user config. Comments and key order must survive agora's edits.
model = "gpt-5.1-codex"  # default model for new threads
approval_policy = "on-request"

[projects."/x"]
trust_level = "trusted"

# A server the user runs by hand.
[mcp_servers.other]
command = "/usr/local/bin/other-mcp"
args = ["--stdio"]

[mcp_servers.other.env]
OTHER_TOKEN_NAME = "OTHER_TOKEN"

[hooks.state."/Users/me/.codex/hooks.json:session_start:0:0"]
trusted_hash = "sha256:1111"
enabled = true
"#;

const AGORA: &str = "/Users/me/.cargo/bin/agora";
const USER_HOOK_KEY: &str = "/Users/me/.codex/hooks.json:session_start:0:0";
const OUR_KEY: &str = "/Users/me/.codex/hooks.json:stop:0:0";

fn agora_args() -> Vec<String> {
    vec!["--home".into(), "/Users/me/.agora".into(), "mcp".into()]
}

fn set(doc: &str) -> (String, ServerChange) {
    set_mcp_server(doc, "agora", AGORA, &agora_args(), None).expect("set succeeds")
}

fn parsed(text: &str) -> DocumentMut {
    text.parse::<DocumentMut>().expect("output parses as TOML")
}

/// Every line of `original` is still in `out`, in the same order (the user's text is kept).
fn assert_lines_kept(original: &str, out: &str) {
    let mut remaining = out.lines();
    for line in original.lines() {
        assert!(
            remaining.any(|l| l == line),
            "line {line:?} lost or reordered in:\n{out}"
        );
    }
}

#[test]
fn add_keeps_user_text_byte_for_byte_and_writes_server() {
    let (out, change) = set(USER);
    assert_eq!(change, ServerChange::Added);
    // The new table is grouped with the other `mcp_servers` tables, so the output is not a prefix
    // extension of USER; every original line must still be present, in order, unchanged.
    assert_lines_kept(USER, &out);
    let doc = parsed(&out);
    assert_eq!(doc["mcp_servers"]["agora"]["command"].as_str(), Some(AGORA));
    assert_eq!(doc["mcp_servers"]["agora"]["args"][2].as_str(), Some("mcp"));
    assert_eq!(
        doc["mcp_servers"]["other"]["command"].as_str(),
        Some("/usr/local/bin/other-mcp")
    );
    assert!(out.contains("[mcp_servers.agora]\n"), "{out}");
}

#[test]
fn add_without_mcp_servers_writes_no_empty_parent_header() {
    let (out, change) = set("model = \"gpt-5.1-codex\"\n");
    assert_eq!(change, ServerChange::Added);
    assert!(out.starts_with("model = \"gpt-5.1-codex\"\n"), "{out}");
    assert!(!out.lines().any(|l| l == "[mcp_servers]"), "{out}");
    assert!(out.contains("[mcp_servers.agora]"), "{out}");
}

#[test]
fn second_set_is_unchanged_and_text_identical() {
    let (first, _) = set(USER);
    let (second, change) = set(&first);
    assert_eq!(change, ServerChange::Unchanged);
    assert_eq!(second, first);
}

#[test]
fn stale_agora_command_is_updated_and_user_keys_kept() {
    let doc = format!(
        "{USER}\n[mcp_servers.agora]\ncommand = \"/old/agora\" # pinned path\nargs = [\"mcp\"]\nstartup_timeout_sec = 30\n"
    );
    let (out, change) = set(&doc);
    assert_eq!(change, ServerChange::Updated);
    assert!(
        out.contains(&format!("command = \"{AGORA}\" # pinned path")),
        "{out}"
    );
    let d = parsed(&out);
    assert_eq!(
        d["mcp_servers"]["agora"]["args"]
            .as_array()
            .map(|a| a.len()),
        Some(3)
    );
    assert_eq!(
        d["mcp_servers"]["agora"]["startup_timeout_sec"].as_integer(),
        Some(30)
    );
}

#[test]
fn foreign_table_is_replaced_whole_with_command_and_args() {
    let doc = "[mcp_servers.agora]\nurl = \"https://mcp.example.com/agora\"\nbearer_token_env_var = \"AGORA_TOKEN\"\nstartup_timeout_sec = 9\n\n[mcp_servers.agora.env]\nAGORA_HOME = \"/elsewhere\"\n";
    let (out, change) = set(doc);
    assert_eq!(change, ServerChange::ReplacedForeign);
    let d = parsed(&out);
    let entry = d["mcp_servers"]["agora"]
        .as_table()
        .expect("agora is a table");
    for key in ["url", "bearer_token_env_var", "startup_timeout_sec", "env"] {
        assert!(!entry.contains_key(key), "{key} survived:\n{out}");
    }
    assert!(!out.contains("mcp_servers.agora.env"), "{out}");
    assert!(!out.contains("https://mcp.example.com"), "{out}");
    assert_eq!(entry["command"].as_str(), Some(AGORA));
    assert_eq!(entry["args"].as_array().map(|a| a.len()), Some(3));
}

#[test]
fn scalar_agora_entry_is_a_usage_error() {
    let err = set_mcp_server(
        "[mcp_servers]\nagora = \"x\"\n",
        "agora",
        AGORA,
        &agora_args(),
        None,
    )
    .expect_err("a scalar entry is not ours to remove");
    assert!(matches!(err, CliError::Usage(_)), "{err}");
}

#[test]
fn command_naming_agora_only_in_a_directory_is_foreign() {
    let doc = "[mcp_servers.agora]\ncommand = \"/opt/agora/bin/other-mcp\"\n";
    let (out, change) = set(doc);
    assert_eq!(change, ServerChange::ReplacedForeign);
    assert_eq!(
        parsed(&out)["mcp_servers"]["agora"]["command"].as_str(),
        Some(AGORA)
    );
}

#[test]
fn equal_values_keep_the_users_literal_quoting() {
    let doc = format!(
        "{USER}\n[mcp_servers.agora]\ncommand = '{AGORA}'\nargs = ['--home', '/Users/me/.agora', 'mcp']\ndefault_tools_approval_mode = 'approve'\n"
    );
    let (out, change) =
        set_mcp_server(&doc, "agora", AGORA, &agora_args(), Some("approve")).expect("set succeeds");
    assert_eq!(change, ServerChange::Unchanged);
    assert_eq!(out, doc);
}

#[test]
fn approve_some_sets_the_mode() {
    let (out, _) =
        set_mcp_server(USER, "agora", AGORA, &agora_args(), Some("approve")).expect("set succeeds");
    assert_eq!(
        parsed(&out)["mcp_servers"]["agora"]["default_tools_approval_mode"].as_str(),
        Some("approve")
    );
}

#[test]
fn approve_none_leaves_the_users_mode_alone() {
    let doc = format!(
        "{USER}\n[mcp_servers.agora]\ncommand = \"{AGORA}\"\nargs = [\"--home\", \"/Users/me/.agora\", \"mcp\"]\ndefault_tools_approval_mode = \"prompt\"\n"
    );
    let (out, change) = set(&doc);
    assert_eq!(change, ServerChange::Unchanged);
    assert_eq!(
        parsed(&out)["mcp_servers"]["agora"]["default_tools_approval_mode"].as_str(),
        Some("prompt")
    );
}

#[test]
fn non_table_mcp_servers_is_a_usage_error() {
    let err = set_mcp_server("mcp_servers = 3\n", "agora", AGORA, &agora_args(), None)
        .expect_err("non-table mcp_servers is refused");
    assert!(matches!(err, CliError::Usage(_)), "{err}");
}

#[test]
fn malformed_config_error_names_config_toml() {
    let err = set_mcp_server("model = \n", "agora", AGORA, &agora_args(), None)
        .expect_err("malformed TOML is refused");
    assert!(matches!(err, CliError::Usage(_)), "{err}");
    assert!(err.to_string().contains("config.toml"), "{err}");
}

#[test]
fn remove_after_add_restores_the_original_exactly() {
    let (added, _) = set(USER);
    let removed = remove_mcp_server(&added, "agora").expect("remove succeeds");
    assert_eq!(removed.as_deref(), Some(USER));
}

#[test]
fn remove_keeps_other_servers_and_their_env() {
    let (added, _) = set(USER);
    let removed = remove_mcp_server(&added, "agora")
        .expect("remove succeeds")
        .expect("agora entry existed");
    assert!(removed.contains("[mcp_servers.other.env]"), "{removed}");
    assert!(!removed.contains("mcp_servers.agora"), "{removed}");
}

#[test]
fn remove_absent_or_foreign_entry_is_none() {
    assert_eq!(remove_mcp_server(USER, "agora").expect("parses"), None);
    let foreign = "[mcp_servers.agora]\ncommand = \"/usr/bin/some-other\"\n";
    assert_eq!(remove_mcp_server(foreign, "agora").expect("parses"), None);
}

#[test]
fn remove_keeps_an_explicit_mcp_servers_header() {
    let doc = "[mcp_servers]\n";
    let (added, _) = set(doc);
    let removed = remove_mcp_server(&added, "agora").expect("remove succeeds");
    assert_eq!(removed.as_deref(), Some(doc));
}

#[test]
fn set_trust_writes_quoted_keys_and_keeps_user_entries() {
    let out = set_trust(USER, &[(OUR_KEY.into(), "sha256:abcd".into())]).expect("set succeeds");
    assert_lines_kept(USER, &out);
    assert!(
        out.contains(&format!("[hooks.state.\"{OUR_KEY}\"]")),
        "{out}"
    );
    let d = parsed(&out);
    assert_eq!(
        d["hooks"]["state"][OUR_KEY]["trusted_hash"].as_str(),
        Some("sha256:abcd")
    );
    assert_eq!(
        d["hooks"]["state"][USER_HOOK_KEY]["trusted_hash"].as_str(),
        Some("sha256:1111")
    );
    assert_eq!(
        d["hooks"]["state"][USER_HOOK_KEY]["enabled"].as_bool(),
        Some(true)
    );
}

#[test]
fn set_trust_updates_hash_and_keeps_other_keys_of_the_entry() {
    let doc = format!(
        "{USER}\n[hooks.state.\"{OUR_KEY}\"]\ntrusted_hash = \"sha256:old\"\nenabled = false\n"
    );
    let out = set_trust(&doc, &[(OUR_KEY.into(), "sha256:new".into())]).expect("set succeeds");
    let d = parsed(&out);
    assert_eq!(
        d["hooks"]["state"][OUR_KEY]["trusted_hash"].as_str(),
        Some("sha256:new")
    );
    assert_eq!(
        d["hooks"]["state"][OUR_KEY]["enabled"].as_bool(),
        Some(false)
    );
    assert!(!out.contains("sha256:old"), "{out}");
}

#[test]
fn set_trust_is_idempotent_and_empty_input_keeps_text() {
    let entries = [(OUR_KEY.to_string(), "sha256:abcd".to_string())];
    let once = set_trust(USER, &entries).expect("set succeeds");
    let twice = set_trust(&once, &entries).expect("set succeeds");
    assert_eq!(twice, once);
    assert_eq!(set_trust(USER, &[]).expect("set succeeds"), USER);
}

#[test]
fn trust_set_then_remove_is_byte_equal_on_a_plain_config() {
    let plain =
        "model = \"gpt-5.1-codex\"  # default\n\n[projects.\"/x\"]\ntrust_level = \"trusted\"\n";
    let set = set_trust(plain, &[(OUR_KEY.into(), "sha256:abcd".into())]).expect("set succeeds");
    let removed = remove_trust(&set, &[OUR_KEY.into()]).expect("remove succeeds");
    assert_eq!(removed.as_deref(), Some(plain));
}

#[test]
fn trust_set_then_remove_is_byte_equal_with_user_hook_state() {
    let set = set_trust(USER, &[(OUR_KEY.into(), "sha256:abcd".into())]).expect("set succeeds");
    let removed = remove_trust(&set, &[OUR_KEY.into()]).expect("remove succeeds");
    assert_eq!(removed.as_deref(), Some(USER));
}

#[test]
fn remove_trust_keeps_the_users_own_entries() {
    let set = set_trust(USER, &[(OUR_KEY.into(), "sha256:abcd".into())]).expect("set succeeds");
    let removed = remove_trust(&set, &[OUR_KEY.into()])
        .expect("remove succeeds")
        .expect("our entry existed");
    assert!(
        removed.contains(&format!("[hooks.state.\"{USER_HOOK_KEY}\"]")),
        "{removed}"
    );
    assert!(
        removed.contains("trusted_hash = \"sha256:1111\""),
        "{removed}"
    );
    assert!(!removed.contains(OUR_KEY), "{removed}");
}

#[test]
fn remove_trust_keeps_other_keys_of_our_entry_and_drops_only_the_hash() {
    let doc = format!(
        "{USER}\n[hooks.state.\"{OUR_KEY}\"]\ntrusted_hash = \"sha256:old\"\nenabled = false\n"
    );
    let removed = remove_trust(&doc, &[OUR_KEY.into()])
        .expect("remove succeeds")
        .expect("our entry existed");
    let d = parsed(&removed);
    assert_eq!(
        d["hooks"]["state"][OUR_KEY]["enabled"].as_bool(),
        Some(false)
    );
    assert!(
        d["hooks"]["state"][OUR_KEY].get("trusted_hash").is_none(),
        "{removed}"
    );
}

#[test]
fn remove_trust_absent_key_is_none() {
    assert_eq!(
        remove_trust(USER, &["/nope:stop:0:0".into()]).expect("parses"),
        None
    );
}

#[test]
fn malformed_config_is_a_usage_error_in_every_function() {
    let bad = "[mcp_servers\n";
    let keys = ["/k:stop:0:0".to_string()];
    let entries = [("/k:stop:0:0".to_string(), "sha256:1".to_string())];
    assert!(matches!(
        set_mcp_server(bad, "agora", AGORA, &agora_args(), None),
        Err(CliError::Usage(_))
    ));
    assert!(matches!(
        remove_mcp_server(bad, "agora"),
        Err(CliError::Usage(_))
    ));
    assert!(matches!(set_trust(bad, &entries), Err(CliError::Usage(_))));
    assert!(matches!(remove_trust(bad, &keys), Err(CliError::Usage(_))));
}
