use serde_json::json;

use super::*;

fn load(documents: &[Value], specs: &[&str]) -> Result<Vec<Task>, InputError> {
    let specs: Vec<String> = specs.iter().map(|spec| (*spec).to_owned()).collect();
    load_tasks(documents, &specs)
}

fn load_error(documents: &[Value], specs: &[&str]) -> String {
    load(documents, specs)
        .expect_err("expected an input error")
        .0
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn tier_of_is_case_insensitive_and_exact() {
    for kind in [
        "SECURITY",
        "Data-Loss",
        "crash",
        "validity",
        "Correctness",
        "safety",
    ] {
        assert_eq!(tier_of(kind), 0, "{kind}");
    }
    for kind in [
        "",
        "other",
        " crash",
        "crashes",
        "data loss",
        "docs",
        "structure",
    ] {
        assert_eq!(tier_of(kind), 1, "{kind:?}");
    }
}

#[test]
fn py_str_of_scalars_matches_python_str() {
    assert_eq!(py_str(&json!(null)), "None");
    assert_eq!(py_str(&json!(true)), "True");
    assert_eq!(py_str(&json!(false)), "False");
    assert_eq!(py_str(&json!(3)), "3");
    assert_eq!(py_str(&json!(-7)), "-7");
    assert_eq!(py_str(&json!(2.5)), "2.5");
    assert_eq!(py_str(&json!(1.0)), "1.0");
    assert_eq!(py_str(&json!(1e16)), "1e+16");
    assert_eq!(py_str(&json!("it's")), "it's");
}

#[test]
fn py_str_of_containers_uses_python_repr() {
    assert_eq!(py_str(&json!([1, "a", null, true])), "[1, 'a', None, True]");
    assert_eq!(py_str(&json!({"a": 1})), "{'a': 1}");
    assert_eq!(py_str(&json!(["it's"])), "[\"it's\"]");
    assert_eq!(py_str(&json!([[], {}])), "[[], {}]");
    assert_eq!(py_str(&json!([2.5, 1e-5])), "[2.5, 1e-05]");
}

#[test]
fn parse_spec_splits_only_before_a_key() {
    let fields = parse_spec("id=b,after=a,title=Fix the parser, then the lexer,kind=validity")
        .expect("valid spec");
    assert_eq!(
        fields,
        pairs(&[
            ("id", "b"),
            ("after", "a"),
            ("title", "Fix the parser, then the lexer"),
            ("kind", "validity"),
        ])
    );
}

#[test]
fn parse_spec_keeps_commas_that_no_key_follows() {
    assert_eq!(
        parse_spec("id=sp, kind=validity").expect("valid spec"),
        pairs(&[("id", "sp, kind=validity")])
    );
    assert_eq!(
        parse_spec("a=1,v2=3").expect("valid spec"),
        pairs(&[("a", "1,v2=3")])
    );
}

#[test]
fn parse_spec_strips_keys_and_values() {
    let fields = parse_spec("id= padded ,files= x + y ,after= a+ ,size= 2 ").expect("valid spec");
    assert_eq!(
        fields,
        pairs(&[
            ("id", "padded"),
            ("files", "x + y"),
            ("after", "a+"),
            ("size", "2")
        ])
    );
}

#[test]
fn parse_spec_rejects_a_piece_without_equals_with_python_reprs() {
    let error = parse_spec("nonsense").expect_err("no key");
    assert_eq!(error.0, "--task 'nonsense': 'nonsense' is not key=value");
    let error = parse_spec("it's").expect_err("no key");
    assert_eq!(error.0, "--task \"it's\": \"it's\" is not key=value");
}

#[test]
fn parse_spec_ignores_empty_keys_and_keeps_duplicates_in_order() {
    assert_eq!(
        parse_spec("=x,id=a,kind=x,kind=validity").expect("valid spec"),
        pairs(&[("id", "a"), ("kind", "x"), ("kind", "validity")])
    );
}

#[test]
fn load_tasks_title_may_hold_commas() {
    let tasks = load(
        &[],
        &["id=a,title=Fix the parser, then the lexer,kind=validity"],
    )
    .expect("valid tasks");
    assert_eq!(tasks[0].title, "Fix the parser, then the lexer");
    assert_eq!(tasks[0].kind, "validity");
    assert_eq!(tasks[0].tier, 0);
}

#[test]
fn load_tasks_reads_the_fixture_spec_fields() {
    let tasks = load(
        &[],
        &["id=b,after=a,title=Fix the parser, then the lexer,kind=validity,queued=7,size=2,value=3.5"],
    )
    .expect("valid tasks");
    let task = &tasks[0];
    assert_eq!(task.index, 0);
    assert_eq!(task.id, "b");
    assert_eq!(task.after, ["a"]);
    assert_eq!(task.queued, Some(Queued::Float(7.0)));
    assert_eq!(task.size, 2.0);
    assert_eq!(task.value, 3.5);
    assert!(task.files.is_empty());
}

#[test]
fn load_tasks_trims_the_id_and_splits_plus_lists() {
    let tasks = load(&[], &["id= padded ,files= x + y ,after= a+ ,size= 2 "]).expect("valid tasks");
    let task = &tasks[0];
    assert_eq!(task.id, "padded");
    assert_eq!(task.files, ["x ", " y"]);
    assert_eq!(task.after, ["a"]);
    assert_eq!(task.size, 2.0);
    assert_eq!(task.value, 1.0);
    assert_eq!(task.kind, "other");
    assert_eq!(task.tier, 1);
    assert_eq!(task.queued, None);
}

#[test]
fn load_tasks_stringifies_null_and_non_string_fields_like_python_str() {
    let tasks = load(
        &[json!([
            {"id": 7, "title": null, "kind": false},
            {"id": null, "kind": ["Crash"]},
            {"id": 2.5}
        ])],
        &[],
    )
    .expect("valid tasks");
    assert_eq!(tasks[0].id, "7");
    assert_eq!(tasks[0].title, "None");
    assert_eq!(tasks[0].kind, "False");
    assert_eq!(tasks[0].tier, 1);
    assert_eq!(tasks[1].id, "None");
    assert_eq!(tasks[1].kind, "['Crash']");
    assert_eq!(tasks[2].id, "2.5");
}

#[test]
fn load_tasks_reports_a_missing_id_by_position() {
    let documents = [json!([{"id": "a"}, {"title": "no id"}])];
    assert_eq!(load_error(&documents, &[]), "task at position 2 has no id");
    assert_eq!(
        load_error(&[json!([{"id": "  "}])], &[]),
        "task at position 1 has no id"
    );
    assert_eq!(
        load_error(&[], &["kind=validity"]),
        "task at position 1 has no id"
    );
}

#[test]
fn load_tasks_sizes_and_values_must_be_positive_numbers() {
    assert_eq!(
        load_error(&[], &["id=a,size=0"]),
        "task a: size must be greater than zero"
    );
    assert_eq!(
        load_error(&[], &["id=a,size=-1"]),
        "task a: size must be greater than zero"
    );
    assert_eq!(
        load_error(&[], &["id=a,size=nan"]),
        "task a: size must be greater than zero"
    );
    assert_eq!(
        load_error(&[], &["id=a,value=lots"]),
        "task a: value must be a number, not 'lots'"
    );
    assert_eq!(
        load_error(&[json!([{"id": "a", "value": -0.5}])], &[]),
        "task a: value must be greater than zero"
    );
}

#[test]
fn load_tasks_absent_size_defaults_but_null_is_an_error() {
    let tasks = load(&[json!([{"id": "a"}])], &[]).expect("valid tasks");
    assert_eq!(tasks[0].size, 1.0);
    assert_eq!(
        load_error(&[json!([{"id": "a", "size": null}])], &[]),
        "task a: size must be a number, not None"
    );
}

#[test]
fn load_tasks_accepts_booleans_and_python_float_text() {
    let tasks = load(
        &[json!([{"id": "a", "value": true}])],
        &["id=b,size=1_0", "id=c,size= 2 "],
    )
    .expect("valid tasks");
    assert_eq!(tasks[0].value, 1.0);
    assert_eq!(tasks[1].size, 10.0);
    assert_eq!(tasks[2].size, 2.0);
}

#[test]
fn list_fields_reject_numbers_bools_and_objects() {
    assert_eq!(
        load_error(&[json!([{"id": "a", "after": 5}])], &[]),
        "expected a list, not 5"
    );
    assert_eq!(
        load_error(&[json!([{"id": "a", "files": true}])], &[]),
        "expected a list, not True"
    );
    assert_eq!(
        load_error(&[json!([{"id": "a", "after": {"a": 1}}])], &[]),
        "expected a list, not {'a': 1}"
    );
}

#[test]
fn list_fields_stringify_json_items_and_keep_empty_ones() {
    let tasks = load(
        &[json!([
            {"id": "a", "after": [1, null, "b"], "files": ""},
            {"id": "b", "files": ["", "x"]}
        ])],
        &[],
    )
    .expect("valid tasks");
    assert_eq!(tasks[0].after, ["1", "None", "b"]);
    assert!(tasks[0].files.is_empty());
    assert_eq!(tasks[1].files, ["", "x"]);
}

#[test]
fn queued_values_are_numbers_or_text() {
    let numbers = load(
        &[json!([
            {"id": "a", "queued": 2},
            {"id": "b", "queued": 1.5},
            {"id": "c", "queued": null},
            {"id": "d", "queued": "7"}
        ])],
        &["id=e,queued=1e1", "id=f,queued= 3 "],
    )
    .expect("valid tasks");
    assert_eq!(numbers[0].queued, Some(Queued::Int(2)));
    assert_eq!(numbers[1].queued, Some(Queued::Float(1.5)));
    assert_eq!(numbers[2].queued, None);
    assert_eq!(numbers[3].queued, Some(Queued::Float(7.0)));
    assert_eq!(numbers[4].queued, Some(Queued::Float(10.0)));
    assert_eq!(numbers[5].queued, Some(Queued::Float(3.0)));

    let text = load(
        &[json!([
            {"id": "a", "queued": "2026-9-24"},
            {"id": "b", "queued": ""}
        ])],
        &[],
    )
    .expect("valid tasks");
    assert_eq!(text[0].queued, Some(Queued::Text("2026-9-24".to_owned())));
    assert_eq!(text[1].queued, Some(Queued::Text(String::new())));
}

#[test]
fn queued_rejects_bool_array_and_object() {
    let message = "task a: queued must be a number or an ISO 8601 time";
    for queued in [json!(true), json!([1]), json!({"at": 1})] {
        assert_eq!(
            load_error(&[json!([{"id": "a", "queued": queued}])], &[]),
            message
        );
    }
}

#[test]
fn arrivals_cannot_mix_numbers_and_timestamps() {
    let message = "queued values must be all numbers or all timestamps";
    assert_eq!(
        load_error(&[], &["id=a,queued=1", "id=b,queued=2026-09-24"]),
        message
    );
    assert_eq!(
        load_error(
            &[json!([{"id": "a", "queued": 1}, {"id": "b", "queued": "2026-09-24"}])],
            &[]
        ),
        message
    );
    // Numeric text counts as a number, so it may sit beside a number.
    assert!(
        load(
            &[json!([{"id": "a", "queued": 1}, {"id": "b", "queued": "2"}])],
            &[]
        )
        .is_ok()
    );
}

#[test]
fn document_must_be_a_list_or_an_object_with_a_tasks_list() {
    let message = "the JSON must be a list of tasks or an object with a \"tasks\" list";
    for document in [
        json!({"x": 1}),
        json!({"tasks": null}),
        json!({"tasks": {"id": "a"}}),
        json!(5),
        json!("text"),
        json!(null),
    ] {
        assert_eq!(load_error(&[document], &[]), message);
    }
    assert!(load(&[json!({"tasks": [{"id": "a"}], "note": 1})], &[]).is_ok());
}

#[test]
fn each_task_in_a_document_must_be_an_object() {
    let message = "each task in the JSON must be an object";
    assert_eq!(load_error(&[json!([1])], &[]), message);
    assert_eq!(load_error(&[json!({"tasks": ["a"]})], &[]), message);
    assert_eq!(load_error(&[json!([{"id": "a"}, "b"])], &[]), message);
}

#[test]
fn documents_then_specs_keep_their_order_and_positions() {
    let tasks = load(
        &[json!([{"id": "a"}]), json!({"tasks": [{"id": "b"}]})],
        &["id=c"],
    )
    .expect("valid tasks");
    let ids: Vec<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
    assert_eq!(ids, ["a", "b", "c"]);
    let positions: Vec<usize> = tasks.iter().map(|task| task.index).collect();
    assert_eq!(positions, [0, 1, 2]);
}

#[test]
fn field_errors_follow_python_precedence() {
    // Order of checks: id, queued type, after, size, value, files.
    assert_eq!(
        load_error(&[json!([{"id": "a", "queued": true, "size": 0}])], &[]),
        "task a: queued must be a number or an ISO 8601 time"
    );
    assert_eq!(
        load_error(&[json!([{"id": "a", "after": 5, "size": 0}])], &[]),
        "expected a list, not 5"
    );
    assert_eq!(
        load_error(&[json!([{"id": "a", "size": 0, "files": 5}])], &[]),
        "task a: size must be greater than zero"
    );
    assert_eq!(
        load_error(&[json!([{"id": "a", "value": "x", "files": 5}])], &[]),
        "task a: value must be a number, not 'x'"
    );
    // The first failing task wins, even when a later task is also broken.
    assert_eq!(
        load_error(&[json!([{"id": "a", "size": 0}, {"title": "no id"}])], &[]),
        "task a: size must be greater than zero"
    );
}
