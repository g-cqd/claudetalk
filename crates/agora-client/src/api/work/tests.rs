use super::paths::canonical_file;
use super::*;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

fn task_with(command: Option<&[&str]>, cwd: Option<&str>) -> TaskRecord {
    serde_json::from_value(json!({
        "seq": 1, "id": "t-1", "title": "x", "kind": "other", "tier": 0,
        "author": "aa", "author_name": "alice", "state": "queued",
        "size": 1.0, "value": 1.0, "weight": 1.0, "queued_ms": 0,
        "command": command, "cwd": cwd, "files": ["/w/a"],
    }))
    .expect("minimal task record")
}

/// The real (symlink-free) form of an existing directory or file.
fn real(path: &Path) -> String {
    std::fs::canonicalize(path)
        .expect("canonical test path")
        .to_string_lossy()
        .into_owned()
}

#[test]
fn canonical_file_resolves_an_existing_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a.txt");
    std::fs::write(&file, b"x").expect("write");
    let got = canonical_file(&file.to_string_lossy(), "/", None).expect("canonical");
    assert_eq!(got, real(&file));
}

#[test]
fn canonical_file_appends_the_missing_tail_to_the_longest_existing_prefix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = real(dir.path());
    let raw = dir.path().join("new").join("deep").join("x.txt");
    let got = canonical_file(&raw.to_string_lossy(), "/", None).expect("canonical");
    assert_eq!(got, format!("{base}/new/deep/x.txt"));
}

#[test]
fn canonical_file_resolves_relative_paths_against_the_base_and_normalises_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = real(dir.path());
    std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
    let up = canonical_file("sub/../f.txt", &base, None).expect("canonical");
    assert_eq!(up, format!("{base}/f.txt"));
    let dots = canonical_file("./sub//g.txt", &base, None).expect("canonical");
    assert_eq!(dots, format!("{base}/sub/g.txt"));
}

#[test]
fn canonical_file_expands_home_only_for_tilde_and_tilde_slash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = real(dir.path());
    let inside = canonical_file("~/f.txt", "/elsewhere", Some(&home)).expect("canonical");
    assert_eq!(inside, format!("{home}/f.txt"));
    let alone = canonical_file("~", "/elsewhere", Some(&home)).expect("canonical");
    assert_eq!(alone, home);
    let other_user = canonical_file("~bob/f", &home, Some(&home)).expect("canonical");
    assert_eq!(other_user, format!("{home}/~bob/f"));
}

#[test]
fn canonical_file_rejects_empty_and_nul_paths() {
    assert!(matches!(
        canonical_file("", "/", None),
        Err(ClientError::Usage(_))
    ));
    assert!(matches!(
        canonical_file("a\0b", "/", None),
        Err(ClientError::Usage(_))
    ));
}

#[cfg(unix)]
#[test]
fn canonical_file_follows_a_symlink_in_the_existing_prefix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = real(dir.path());
    std::fs::create_dir(dir.path().join("real")).expect("mkdir");
    std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).expect("symlink");
    let got = canonical_file(&format!("{root}/link/new.txt"), "/", None).expect("canonical");
    assert_eq!(got, format!("{root}/real/new.txt"));
}

#[test]
fn canonicalise_add_defaults_the_cwd_and_resolves_files_against_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = real(dir.path());
    let mut a = AddArgs {
        files: vec!["f.txt".to_string(), format!("{base}/abs/g")],
        ..AddArgs::default()
    };
    canonicalise_add(&mut a, &base, None).expect("canonical");
    assert_eq!(a.cwd, Some(base.clone()));
    assert_eq!(
        a.files,
        vec![format!("{base}/f.txt"), format!("{base}/abs/g")]
    );
}

#[test]
fn canonicalise_add_resolves_a_relative_cwd_against_the_base() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = real(dir.path());
    let mut a = AddArgs {
        cwd: Some("sub".to_string()),
        files: vec!["f.txt".to_string()],
        ..AddArgs::default()
    };
    canonicalise_add(&mut a, &base, None).expect("canonical");
    assert_eq!(a.cwd, Some(format!("{base}/sub")));
    assert_eq!(a.files, vec![format!("{base}/sub/f.txt")]);
}

#[test]
fn select_env_keeps_a_given_env_and_scrubs_the_process_env_otherwise() {
    let given = BTreeMap::from([("X".to_string(), "1".to_string())]);
    assert_eq!(select_env(Some(given.clone()), Vec::new()), given);
    let vars = vec![
        (OsString::from("PATH"), OsString::from("/bin")),
        (OsString::from("GH_TOKEN"), OsString::from("secret")),
        (OsString::from("AGORA_HOME"), OsString::from("/h")),
    ];
    let env = select_env(None, vars);
    assert_eq!(env.keys().map(String::as_str).collect::<Vec<_>>(), ["PATH"]);
}

#[test]
fn select_env_drops_variables_that_are_not_utf8() {
    let vars = vec![
        (OsString::from("OK"), OsString::from("v")),
        (OsString::from("BAD"), OsString::from_vec(vec![0xff])),
    ];
    let env = select_env(None, vars);
    assert_eq!(env.get("OK").map(String::as_str), Some("v"));
    assert!(!env.contains_key("BAD"));
}

#[test]
fn clamp_wait_caps_the_timeout_at_the_protocol_maximum() {
    assert_eq!(clamp_wait(0), 0);
    assert_eq!(clamp_wait(1_000), 1_000);
    assert_eq!(clamp_wait(MAX_WAIT_MS), 55_000);
    assert_eq!(clamp_wait(MAX_WAIT_MS + 1), 55_000);
    assert_eq!(clamp_wait(u64::MAX), 55_000);
}

#[test]
fn with_digest_is_the_claims_digest_of_the_command_or_none() {
    let files = vec!["/w/a".to_string()];
    let t = with_digest(task_with(Some(&["touch", "/x"]), Some("/w")));
    let argv = vec!["touch".to_string(), "/x".to_string()];
    assert_eq!(t.digest, Some(claims::digest(&argv, "/w", &files)));
    let no_cwd = with_digest(task_with(Some(&["ls"]), None));
    assert_eq!(
        no_cwd.digest,
        Some(claims::digest(&["ls".to_string()], "", &files))
    );
    assert_eq!(with_digest(task_with(None, None)).digest, None);
}

#[test]
fn show_json_carries_the_task_and_its_digest() {
    let t = with_digest(task_with(Some(&["ls"]), Some("/w")));
    let v = show_json(&t).expect("json");
    assert_eq!(v["task"]["id"], "t-1");
    assert_eq!(v["digest"], json!(t.digest));
    assert!(v["digest"].is_string());
    let plain = show_json(&task_with(None, None)).expect("json");
    assert!(plain["digest"].is_null());
}

#[test]
fn next_json_is_null_when_nothing_is_claimable() {
    let outcome = NextOutcome {
        claimed: None,
        reason: "nothing ready".to_string(),
    };
    let v = next_json(&outcome).expect("json");
    assert_eq!(v, json!({"claimed": null, "reason": "nothing ready"}));
}

#[test]
fn accepted_json_has_first_and_the_digest() {
    let t = with_digest(task_with(Some(&["ls"]), None));
    let v = accepted_json(&t, true).expect("json");
    assert_eq!(v["first"], true);
    assert_eq!(v["digest"], json!(t.digest));
    assert_eq!(v["task"]["id"], "t-1");
}

#[test]
fn plan_json_parses_the_plan_and_keeps_the_text() {
    let v = plan_json(r#"{"order":[0,1]}"#, "1. a\n2. b").expect("json");
    assert_eq!(v, json!({"plan": {"order": [0, 1]}, "text": "1. a\n2. b"}));
}

#[test]
fn plan_json_rejects_a_plan_that_is_not_json() {
    assert!(matches!(
        plan_json("not json", "t"),
        Err(ClientError::Proto(_))
    ));
}

#[test]
fn absolute_spec_makes_relative_file_and_gone_paths_absolute() {
    let base = || Ok("/w/dir".to_string());
    let file = absolute_spec(
        WaitSpec::File {
            paths: vec!["out/r.status".to_string(), "/abs/x".to_string()],
        },
        base,
    )
    .expect("file spec");
    assert_eq!(
        file,
        WaitSpec::File {
            paths: vec!["/w/dir/out/r.status".to_string(), "/abs/x".to_string()],
        }
    );
    let gone = absolute_spec(
        WaitSpec::Gone {
            paths: vec!["P2.lock".to_string()],
        },
        base,
    )
    .expect("gone spec");
    assert_eq!(
        gone,
        WaitSpec::Gone {
            paths: vec!["/w/dir/P2.lock".to_string()],
        }
    );
}

#[test]
fn absolute_spec_makes_the_log_path_absolute_and_keeps_the_pattern() {
    let log = absolute_spec(
        WaitSpec::Log {
            path: "logs/build.log".to_string(),
            pattern: "Build complete|^done$".to_string(),
        },
        || Ok("/w/dir".to_string()),
    )
    .expect("log spec");
    assert_eq!(
        log,
        WaitSpec::Log {
            path: "/w/dir/logs/build.log".to_string(),
            pattern: "Build complete|^done$".to_string(),
        }
    );
    let absolute = absolute_spec(
        WaitSpec::Log {
            path: "/var/x.log".to_string(),
            pattern: "p".to_string(),
        },
        || Ok("/w/dir".to_string()),
    )
    .expect("log spec");
    assert!(matches!(absolute, WaitSpec::Log { ref path, .. } if path == "/var/x.log"));
}

#[test]
fn absolute_spec_leaves_other_conditions_unchanged_without_reading_the_directory() {
    let refused = || Err(ClientError::Usage("no directory".into()));
    let pid = WaitSpec::Pid { pids: vec![77] };
    assert_eq!(absolute_spec(pid.clone(), refused).expect("pid"), pid);
    let tasks = WaitSpec::Tasks {
        ids: vec!["t-1".to_string()],
        any: true,
    };
    assert_eq!(absolute_spec(tasks.clone(), refused).expect("tasks"), tasks);
}

#[test]
fn absolute_spec_refuses_an_empty_or_nul_path() {
    let base = || Ok("/w".to_string());
    let empty = absolute_spec(
        WaitSpec::File {
            paths: vec![String::new()],
        },
        base,
    );
    assert!(matches!(empty, Err(ClientError::Usage(_))));
    let nul = absolute_spec(
        WaitSpec::Log {
            path: "a\0b".to_string(),
            pattern: "p".to_string(),
        },
        base,
    );
    assert!(matches!(nul, Err(ClientError::Usage(_))));
}

#[test]
fn prepare_add_signs_a_bare_title_without_a_cwd_and_never_reads_the_directory() {
    let mut a = AddArgs {
        title: "think".to_string(),
        ..AddArgs::default()
    };
    prepare_add(
        &mut a,
        || Err(ClientError::Usage("no directory".into())),
        None,
    )
    .expect("bare title");
    assert_eq!(a.cwd, None);
    assert!(a.files.is_empty());
}

#[test]
fn prepare_add_canonicalises_when_a_command_a_cwd_or_files_is_given() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = real(dir.path());
    let with_command = AddArgs {
        command: Some(vec!["ls".to_string()]),
        ..AddArgs::default()
    };
    let with_cwd = AddArgs {
        cwd: Some("sub".to_string()),
        ..AddArgs::default()
    };
    let with_files = AddArgs {
        files: vec!["f.txt".to_string()],
        ..AddArgs::default()
    };
    for mut a in [with_command, with_cwd, with_files] {
        prepare_add(&mut a, || Ok(base.clone()), None).expect("canonical");
        assert!(a.cwd.is_some(), "cwd is set for {:?}", a.command);
    }
}
