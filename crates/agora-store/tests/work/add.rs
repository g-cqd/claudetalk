use super::*;

// ---- adding tasks ----------------------------------------------------------------------------------------------

#[test]
fn a_task_added_with_only_a_scope_starts_queued_with_every_default() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let t = s
        .work_add(NOW, &op(alice, "work.add", "", r#"{"scope":"proj"}"#))
        .expect("add");
    assert_eq!((t.id.as_str(), t.title.as_str()), ("task-1", ""));
    assert_eq!((t.kind.as_str(), t.state), ("other", TaskState::Queued));
    assert_eq!(t.scope.as_deref(), Some(PROJ));
    assert_eq!(t.author, pk_hex(alice));
    assert_eq!((t.size, t.value, t.weight), (1.0, 1.0, 1.0));
    assert_eq!((t.agent, t.accepted_by, t.claimed_ms), (None, None, None));
    assert_eq!((t.requested_ms, t.command, t.cwd), (None, None, None));
    assert_eq!((t.digest, t.note.as_str()), (None, ""));
    assert!(t.after.is_empty() && t.files.is_empty() && !t.needs_acceptance);
}

#[test]
fn queued_tasks_without_an_id_are_numbered_task_n_in_arrival_order() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let first = s
        .work_add(NOW, &op(alice, "work.add", "", r#"{"scope":"proj"}"#))
        .expect("first");
    let second = s
        .work_add(NOW, &op(alice, "work.add", "", r#"{"scope":"proj"}"#))
        .expect("second");
    assert_eq!(
        (first.id.as_str(), second.id.as_str()),
        ("task-1", "task-2")
    );
}

#[test]
fn an_explicit_task_id_is_kept_as_given() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let t = queue(s, alice, "deploy-1", json!({}));
    assert_eq!((t.id.as_str(), t.state), ("deploy-1", TaskState::Queued));
}

#[test]
fn an_automatic_id_skips_an_id_already_taken() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "task-2", json!({}));
    let auto = s
        .work_add(NOW, &op(alice, "work.add", "", r#"{"scope":"proj"}"#))
        .expect("auto");
    assert_eq!(auto.id, "task-3");
}

#[test]
fn a_duplicate_task_id_is_refused() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    queue(s, alice, "t1", json!({}));
    let again = s.work_add(NOW, &op(alice, "work.add", "", &add_args("t1", json!({}))));
    assert_eq!(refusal(again), "task t1 exists already");
}

#[test]
fn a_task_after_an_unknown_task_is_refused() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let add = add_args("t9", json!({"after": ["ghost"]}));
    let msg = refusal(s.work_add(NOW, &op(alice, "work.add", "", &add)));
    assert_eq!(msg, "task t9 is after unknown task ghost");
}

#[test]
fn an_unknown_scope_is_refused_with_the_create_hint() {
    let mut w = world();
    let (s, alice) = (&mut w.s, &w.alice);
    let add = add_args("t1", json!({"scope": "nope"}));
    let msg = refusal(s.work_add(NOW, &op(alice, "work.add", "", &add)));
    assert_eq!(
        msg,
        "unknown scope nope; create it with `agora work scope set nope --kind project`"
    );
}

#[test]
fn invalid_task_ids_are_refused_by_the_name_rule() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let long = "a".repeat(129);
    for id in ["bad id", "", "-lead", long.as_str()] {
        let add = add_args(id, json!({}));
        let msg = refusal(s.work_add(NOW, &op(alice, "work.add", "", &add)));
        assert_eq!(msg, format!("task '{id}' is not valid: {NAME_RULE}"));
    }
}

#[test]
fn an_invalid_kind_is_refused_by_the_kind_rule() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let add = add_args("t1", json!({"kind": "bad kind"}));
    let msg = refusal(s.work_add(NOW, &op(alice, "work.add", "", &add)));
    assert_eq!(
        msg,
        "kind 'bad kind' is not valid: use up to 64 letters, digits, _ or -"
    );
}

#[test]
fn a_zero_size_is_refused() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let add = add_args("t1", json!({"size": 0}));
    let err = s.work_add(NOW, &op(alice, "work.add", "", &add));
    assert_eq!(invalid(err), "size must be a number above zero");
}

#[test]
fn a_relative_file_path_without_a_working_directory_is_refused() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let add = add_args("t1", json!({"files": ["src/a.rs"]}));
    let err = s.work_add(NOW, &op(alice, "work.add", "", &add));
    assert_eq!(
        invalid(err),
        "a relative file path needs a working directory"
    );
}

#[test]
fn file_paths_are_stored_canonical_and_deduplicated() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let t = queue(
        s,
        alice,
        "t1",
        json!({"files": ["/proj/src/../docs//a.md", "/proj/docs/a.md/"]}),
    );
    assert_eq!(t.files, vec!["/proj/docs/a.md".to_string()]);
}

#[test]
fn a_command_needs_an_existing_absolute_working_directory() {
    let mut w = world();
    project(&mut w);
    let (s, alice) = (&mut w.s, &w.alice);
    let no_cwd = add_args("t1", json!({"command": ["true"]}));
    let err = s.work_add(NOW, &op(alice, "work.add", "", &no_cwd));
    assert_eq!(invalid(err), "a command needs a working directory");
    let missing = add_args(
        "t1",
        json!({"command": ["true"], "cwd": "/nonexistent-agora-m5"}),
    );
    let msg = refusal(s.work_add(NOW, &op(alice, "work.add", "", &missing)));
    assert_eq!(msg, "/nonexistent-agora-m5 is not a directory");
}

#[test]
fn malformed_add_arguments_are_refused() {
    let mut w = world();
    let (s, alice) = (&mut w.s, &w.alice);
    let err = s.work_add(NOW, &op(alice, "work.add", "", "not json"));
    assert_eq!(invalid(err), "malformed arguments");
}
