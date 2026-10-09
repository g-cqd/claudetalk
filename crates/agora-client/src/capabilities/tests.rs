use super::*;
use std::os::unix::fs::PermissionsExt;

fn home() -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = Paths::new(dir.path());
    (dir, paths)
}

fn full() -> CapabilitiesFile {
    CapabilitiesFile {
        schema: 1,
        claude: Some(ClaudeCapabilities {
            probed_at_ms: 1_786_000_000_000,
            claude_bin: Some("/usr/local/bin/claude".into()),
            claude_version: Some("2.1.293".into()),
            mods: true,
            channels_flag: true,
            async_rewake: true,
            stop_hook: true,
            context_hook: true,
            mcp_negotiation_legacy: true,
            install_mode: Some("plugin".into()),
            scope: Some("user".into()),
        }),
        other: serde_json::Map::new(),
    }
}

#[test]
fn path_is_under_home() {
    let (dir, paths) = home();
    assert_eq!(path(&paths), dir.path().join(FILE_NAME));
}

#[test]
fn save_then_load_round_trips() {
    let (_dir, paths) = home();
    save(&paths, &full()).expect("save");
    assert_eq!(load(&paths), Some(full()));
}

#[test]
fn unknown_top_level_keys_survive_load_and_save() {
    let (_dir, paths) = home();
    let codex = serde_json::json!({"version": "0.9", "flags": [1, 2]});
    let original = serde_json::json!({
        "schema": 1,
        "claude": {"mods": true, "claude_version": "2.1.293"},
        "codex": codex,
    });
    std::fs::write(path(&paths), original.to_string()).expect("write");

    let loaded = load(&paths).expect("loads");
    assert_eq!(loaded.other.get("codex"), Some(&codex));
    save(&paths, &loaded).expect("save");

    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path(&paths)).expect("read")).expect("json");
    assert_eq!(saved.get("codex"), Some(&codex));
    assert_eq!(saved["claude"]["mods"], serde_json::json!(true));
    assert_eq!(
        saved["claude"]["claude_version"],
        serde_json::json!("2.1.293")
    );
    assert_eq!(saved.get("schema"), Some(&serde_json::json!(1)));
}

#[test]
fn missing_file_loads_as_none() {
    let (_dir, paths) = home();
    assert_eq!(load(&paths), None);
}

#[test]
fn garbage_json_loads_as_none() {
    let (_dir, paths) = home();
    std::fs::write(path(&paths), "not json {").expect("write");
    assert_eq!(load(&paths), None);
}

#[test]
fn oversized_file_loads_as_none() {
    let (_dir, paths) = home();
    let pad = "x".repeat(64 * 1024);
    std::fs::write(path(&paths), format!(r#"{{"schema":1,"pad":"{pad}"}}"#)).expect("write");
    assert_eq!(load(&paths), None);
}

#[test]
fn symlink_loads_as_none_and_save_refuses() {
    let (dir, paths) = home();
    let real = dir.path().join("real.json");
    std::fs::write(&real, r#"{"schema":1}"#).expect("write");
    std::os::unix::fs::symlink(&real, path(&paths)).expect("symlink");

    assert_eq!(load(&paths), None);
    let err = save(&paths, &full()).expect_err("save must refuse a symlink");
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        std::fs::read_to_string(&real).expect("read"),
        r#"{"schema":1}"#
    );
}

#[test]
fn saved_file_is_mode_0600_and_leaves_no_temp_file() {
    let (dir, paths) = home();
    save(&paths, &full()).expect("save");
    let mode = std::fs::metadata(path(&paths))
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![FILE_NAME.to_string()]);
}

#[test]
fn save_without_home_dir_is_not_found() {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = Paths::new(&dir.path().join("missing"));
    let err = save(&paths, &full()).expect_err("no home dir");
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

#[test]
fn default_file_has_schema_1() {
    let file = CapabilitiesFile::default();
    assert_eq!(file.schema, 1);
    assert!(file.claude.is_none());
    assert!(file.other.is_empty());
}

#[test]
fn schema_only_file_loads_without_claude() {
    let (_dir, paths) = home();
    std::fs::write(path(&paths), r#"{"schema":1}"#).expect("write");
    let loaded = load(&paths).expect("loads");
    assert_eq!(loaded.schema, 1);
    assert!(loaded.claude.is_none());
}

#[test]
fn partial_claude_object_loads_with_defaults() {
    let (_dir, paths) = home();
    std::fs::write(path(&paths), r#"{"schema":1,"claude":{"mods":true}}"#).expect("write");
    let loaded = load(&paths).expect("loads");
    assert_eq!(
        loaded.claude,
        Some(ClaudeCapabilities {
            mods: true,
            ..ClaudeCapabilities::default()
        })
    );
}

fn codex_caps() -> HarnessCapabilities {
    HarnessCapabilities {
        probed_at_ms: 1_786_000_000_000,
        bin: Some("/usr/local/bin/codex".into()),
        version: Some("0.157.1".into()),
        scopes: vec!["project:/work/app".into(), "user".into()],
        mcp_configured: true,
        context_hook: true,
        stop_hook: false,
        hooks_trusted: Some(false),
        owned_session: true,
        files: vec![
            "/home/u/.codex/config.toml".into(),
            "/work/app/AGENTS.md".into(),
        ],
        notes: vec!["hooks written, not trusted yet".into()],
    }
}

#[test]
fn claude_codex_and_unknown_entries_survive_load_and_save() {
    let (_dir, paths) = home();
    let codex = serde_json::to_value(codex_caps()).expect("json");
    let original = serde_json::json!({
        "schema": 1,
        "claude": {"mods": true, "claude_version": "2.1.293"},
        "codex": codex,
        "future_key": {"x": [1, 2]},
    });
    std::fs::write(path(&paths), original.to_string()).expect("write");

    let loaded = load(&paths).expect("loads");
    assert_eq!(loaded.harness(Harness::Codex), Some(codex_caps()));
    save(&paths, &loaded).expect("save");

    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path(&paths)).expect("read")).expect("json");
    assert_eq!(saved["codex"], codex);
    assert_eq!(saved["future_key"], serde_json::json!({"x": [1, 2]}));
    assert_eq!(saved["claude"]["mods"], serde_json::json!(true));
    assert_eq!(saved["schema"], serde_json::json!(1));
}

#[test]
fn set_harness_then_remove_round_trips_through_other() {
    let mut file = CapabilitiesFile::default();
    assert_eq!(file.harness(Harness::Codex), None);
    file.set_harness(Harness::Codex, Some(codex_caps()));
    assert_eq!(file.harness(Harness::Codex), Some(codex_caps()));
    assert!(file.other.contains_key("codex"));
    file.set_harness(Harness::Codex, None);
    assert_eq!(file.harness(Harness::Codex), None);
    assert!(!file.other.contains_key("codex"));
}

#[test]
fn set_harness_is_a_no_op_for_claude_code_and_unknown() {
    let mut file = CapabilitiesFile::default();
    file.set_harness(Harness::ClaudeCode, Some(codex_caps()));
    file.set_harness(Harness::Unknown, Some(codex_caps()));
    assert!(file.other.is_empty());
    assert_eq!(file.harness(Harness::ClaudeCode), None);
    assert_eq!(file.harness(Harness::Unknown), None);
}

#[test]
fn garbage_harness_entry_reads_as_none_and_survives_save() {
    let (_dir, paths) = home();
    std::fs::write(path(&paths), r#"{"schema":1,"codex":"oops"}"#).expect("write");
    let loaded = load(&paths).expect("loads");
    assert_eq!(loaded.harness(Harness::Codex), None);
    save(&paths, &loaded).expect("save");
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path(&paths)).expect("read")).expect("json");
    assert_eq!(saved["codex"], serde_json::json!("oops"));
}

/// A file where Codex and Copilot CLI both list `/w/AGENTS.md`, and only Copilot lists its hooks.
fn two_harness_file() -> CapabilitiesFile {
    let mut file = CapabilitiesFile::default();
    file.set_harness(
        Harness::Codex,
        Some(HarnessCapabilities {
            files: vec!["/w/AGENTS.md".into(), "/h/codex.json".into()],
            ..HarnessCapabilities::default()
        }),
    );
    file.set_harness(
        Harness::CopilotCli,
        Some(HarnessCapabilities {
            files: vec!["/w/AGENTS.md".into(), "/w/.github/hooks.json".into()],
            ..HarnessCapabilities::default()
        }),
    );
    file
}

#[test]
fn file_shared_is_true_when_another_harness_lists_the_path() {
    let file = two_harness_file();
    assert!(file.file_shared("/w/AGENTS.md", Harness::Cursor));
    assert!(file.file_shared("/w/AGENTS.md", Harness::Codex));
    assert!(file.file_shared("/w/.github/hooks.json", Harness::Codex));
}

#[test]
fn file_shared_is_false_for_the_except_harness_unlisted_or_inexact_paths() {
    let file = two_harness_file();
    assert!(!file.file_shared("/w/.github/hooks.json", Harness::CopilotCli));
    assert!(!file.file_shared("/h/codex.json", Harness::Codex));
    assert!(!file.file_shared("/w/AGENTS", Harness::Cursor));
    assert!(!file.file_shared("/w/missing", Harness::Cursor));
}

#[test]
fn file_shared_never_counts_claude_code_or_unknown_entries() {
    let mut file = CapabilitiesFile::default();
    let listing = serde_json::json!({"files": ["/w/AGENTS.md"]});
    file.other
        .insert(Harness::ClaudeCode.as_str().to_owned(), listing.clone());
    file.other
        .insert(Harness::Unknown.as_str().to_owned(), listing);
    assert!(!file.file_shared("/w/AGENTS.md", Harness::Codex));
}

#[test]
fn partial_harness_entry_fills_defaults() {
    let mut file = CapabilitiesFile::default();
    file.other
        .insert("codex".to_owned(), serde_json::json!({"stop_hook": true}));
    assert_eq!(
        file.harness(Harness::Codex),
        Some(HarnessCapabilities {
            stop_hook: true,
            ..HarnessCapabilities::default()
        })
    );
    let caps = file.harness(Harness::Codex).expect("parses");
    assert!(!caps.context_hook);
    assert_eq!(caps.hooks_trusted, None);
    assert!(caps.files.is_empty());
}

#[test]
fn save_refuses_a_file_larger_than_64_kib() {
    let (dir, paths) = home();
    let mut file = CapabilitiesFile::default();
    file.other.insert(
        "pad".to_owned(),
        serde_json::Value::String("x".repeat(64 * 1024)),
    );
    let err = save(&paths, &file).expect_err("oversized save must fail");
    assert_eq!(err.kind(), ErrorKind::InvalidData);
    assert!(!path(&paths).exists());
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.is_empty());
}
