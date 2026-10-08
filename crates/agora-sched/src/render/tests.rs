use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};

use super::*;
use crate::task::load_tasks;

fn graph(documents: &[Value], specs: &[&str]) -> Graph {
    let specs: Vec<String> = specs.iter().map(|spec| (*spec).to_owned()).collect();
    let tasks = load_tasks(documents, &specs).expect("valid task list");
    Graph::new(tasks).expect("valid graph")
}

fn one_document(tasks: Value) -> Graph {
    graph(&[tasks], &[])
}

/// old(performance), refactor(structure), fix(validity, after refactor): fix moves refactor up.
fn moved_up_example() -> Graph {
    one_document(json!([
        {"id": "old", "kind": "performance"},
        {"id": "refactor", "kind": "structure"},
        {"id": "fix", "kind": "validity", "after": ["refactor"]}
    ]))
}

fn value<'a>(iter: &mut std::slice::Iter<'a, String>) -> &'a str {
    iter.next().expect("a value after the flag").as_str()
}

struct Invocation {
    mode: Mode,
    policy: OrderPolicy,
    json: bool,
    specs: Vec<String>,
}

/// Parses the subset of `work order|plan` arguments the fixtures use.
fn invocation(args: &[String]) -> Invocation {
    let mut command = "order";
    let mut policy = OrderPolicy::Fifo;
    let mut agents = 1_usize;
    let mut assign_flag = false;
    let mut conflict_cost: Option<f64> = None;
    let mut json_flag = false;
    let mut specs = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "order" | "plan" => command = arg.as_str(),
            "--json" => json_flag = true,
            "--assign" => assign_flag = true,
            "--policy" => {
                policy = OrderPolicy::parse(value(&mut iter)).expect("known policy");
            }
            "--agents" => agents = value(&mut iter).parse::<usize>().expect("agents"),
            "--conflict-cost" => {
                conflict_cost = Some(value(&mut iter).parse::<f64>().expect("cost"));
            }
            "--task" => specs.push(value(&mut iter).to_owned()),
            other => panic!("unexpected fixture argument {other}"),
        }
    }
    let mode = if command == "plan" {
        Mode::Plan {
            agents,
            assign: assign_flag,
            conflict_cost,
        }
    } else {
        Mode::Order
    };
    Invocation {
        mode,
        policy,
        json: json_flag,
        specs,
    }
}

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ordering")
}

#[test]
fn fixtures_render_exact_stdout() {
    let mut paths: Vec<PathBuf> = fs::read_dir(fixture_dir())
        .expect("fixture directory")
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    paths.sort();
    let mut checked = 0_usize;
    for path in paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".input.json") else {
            continue;
        };
        if !["order_", "plan_", "assign_"]
            .iter()
            .any(|p| stem.starts_with(p))
        {
            continue;
        }
        let input: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("readable input fixture"))
                .expect("input fixture is JSON");
        let expected: Value = serde_json::from_str(
            &fs::read_to_string(fixture_dir().join(format!("{stem}.expected.json")))
                .expect("readable expected fixture"),
        )
        .expect("expected fixture is JSON");
        assert_eq!(expected["exit_code"], json!(0), "{stem} is an error case");

        let args: Vec<String> = input["args"]
            .as_array()
            .expect("args array")
            .iter()
            .map(|arg| arg.as_str().expect("string argument").to_owned())
            .collect();
        let tasks = input["tasks"].clone();
        assert!(!tasks.is_null(), "{stem} has no inline tasks");
        let run = invocation(&args);
        let specs: Vec<&str> = run.specs.iter().map(String::as_str).collect();
        let g = graph(&[tasks], &specs);
        let rendered = render(&g, run.policy, &run.mode, run.json);
        let stdout = expected["stdout"].as_str().expect("stdout string");
        assert_eq!(format!("{rendered}\n"), stdout, "fixture {stem}");
        checked += 1;
    }
    assert!(checked >= 12, "only {checked} fixture cases were checked");
}

#[test]
fn order_json_is_exact_for_a_moved_up_prerequisite() {
    let g = moved_up_example();
    let expected = "{\n  \"policy\": \"fifo\",\n  \"order\": [\n    {\n      \"id\": \"refactor\",\n      \"tier\": 1,\n      \"moved_up_for\": \"fix\"\n    },\n    {\n      \"id\": \"fix\",\n      \"tier\": 0,\n      \"moved_up_for\": null\n    },\n    {\n      \"id\": \"old\",\n      \"tier\": 1,\n      \"moved_up_for\": null\n    }\n  ]\n}";
    assert_eq!(render(&g, OrderPolicy::Fifo, &Mode::Order, true), expected);
}

#[test]
fn order_text_names_the_kind_and_the_task_that_moved_it() {
    let g = moved_up_example();
    let text = render(&g, OrderPolicy::Fifo, &Mode::Order, false);
    assert_eq!(
        text,
        "1. refactor (structure, moved up for fix)\n2. fix (validity)\n3. old (performance)"
    );
}

#[test]
fn text_has_no_trailing_newline() {
    let g = moved_up_example();
    assert!(!render(&g, OrderPolicy::Fifo, &Mode::Order, false).ends_with('\n'));
    let plan = Mode::Plan {
        agents: 2,
        assign: false,
        conflict_cost: None,
    };
    assert!(!render(&g, OrderPolicy::Fifo, &plan, false).ends_with('\n'));
}

#[test]
fn queued_values_print_as_python_str() {
    assert_eq!(queued_text(&Queued::Int(3)), "3");
    assert_eq!(queued_text(&Queued::Float(2.5)), "2.5");
    assert_eq!(queued_text(&Queued::Float(7.0)), "7.0");
    assert_eq!(queued_text(&Queued::Float(1e16)), "1e+16");
    assert_eq!(queued_text(&Queued::Float(f64::INFINITY)), "inf");
    assert_eq!(
        queued_text(&Queued::Text("2026-9-24".to_owned())),
        "2026-9-24"
    );
}

#[test]
fn text_puts_title_before_and_queued_after_the_kind() {
    let g = one_document(json!([{"id": "a", "title": "Fix it", "queued": 3}]));
    let text = render(&g, OrderPolicy::Fifo, &Mode::Order, false);
    assert_eq!(text, "1. a Fix it (other, queued 3)");
}

#[test]
fn text_escapes_control_characters_in_titles() {
    let g = one_document(json!([{"id": "a", "title": "x\u{1b}[2Jy"}]));
    let text = render(&g, OrderPolicy::Fifo, &Mode::Order, false);
    assert_eq!(text, "1. a x\\x1b[2Jy (other)");
    assert!(!text.contains('\u{1b}'));
}

#[test]
fn one_agent_plan_renders_the_order_shape() {
    let g = moved_up_example();
    let plan = Mode::Plan {
        agents: 1,
        assign: false,
        conflict_cost: None,
    };
    assert_eq!(
        render(&g, OrderPolicy::Fifo, &plan, true),
        render(&g, OrderPolicy::Fifo, &Mode::Order, true)
    );
    assert_eq!(
        render(&g, OrderPolicy::Fifo, &plan, false),
        render(&g, OrderPolicy::Fifo, &Mode::Order, false)
    );
}

#[test]
fn schedule_text_marks_idle_lanes_and_both_bounds() {
    let g = one_document(json!([{"id": "a", "size": 2}]));
    let plan = Mode::Plan {
        agents: 3,
        assign: false,
        conflict_cost: None,
    };
    assert_eq!(
        render(&g, OrderPolicy::Fifo, &plan, false),
        "agent 1: a [0-2]\nagent 2: idle\nagent 3: idle\nmakespan 2; lower bound 2"
    );
}

#[test]
fn schedule_json_uses_float_times_and_empty_idle_lanes() {
    let g = one_document(json!([{"id": "a", "size": 2}]));
    let plan = Mode::Plan {
        agents: 3,
        assign: false,
        conflict_cost: None,
    };
    let expected = "{\n  \"agents\": 3,\n  \"makespan\": 2.0,\n  \"lower_bound\": 2.0,\n  \"lanes\": [\n    [\n      {\n        \"id\": \"a\",\n        \"start\": 0.0,\n        \"end\": 2.0\n      }\n    ],\n    [],\n    []\n  ]\n}";
    assert_eq!(render(&g, OrderPolicy::Fifo, &plan, true), expected);
}

#[test]
fn assign_text_names_agents_and_the_equilibrium() {
    let g = one_document(json!([{"id": "a"}, {"id": "b"}]));
    let plan = Mode::Plan {
        agents: 3,
        assign: true,
        conflict_cost: None,
    };
    assert_eq!(
        render(&g, OrderPolicy::Fifo, &plan, false),
        "agent 1: a\nagent 2: b\nagent 3: idle\nNash equilibrium after 1 rounds; potential 3"
    );
}

#[test]
fn assign_json_types_idle_and_shared_costs_as_floats() {
    let g = one_document(json!([{"id": "a"}, {"id": "b"}]));
    let plan = Mode::Plan {
        agents: 3,
        assign: true,
        conflict_cost: None,
    };
    let expected = "{\n  \"agents\": 3,\n  \"assignment\": [\n    {\n      \"agent\": 0,\n      \"task\": \"a\",\n      \"cost\": 0.0\n    },\n    {\n      \"agent\": 1,\n      \"task\": \"b\",\n      \"cost\": 1.0\n    },\n    {\n      \"agent\": 2,\n      \"task\": null,\n      \"cost\": 2.0\n    }\n  ],\n  \"potential\": 3.0,\n  \"rounds\": 1,\n  \"equilibrium\": true\n}";
    assert_eq!(render(&g, OrderPolicy::Fifo, &plan, true), expected);
}

#[test]
fn empty_task_list_renders_empty_text_and_an_empty_order() {
    let g = graph(&[json!([])], &[]);
    assert_eq!(render(&g, OrderPolicy::Fifo, &Mode::Order, false), "");
    assert!(render(&g, OrderPolicy::Fifo, &Mode::Order, true).contains("\"order\": []"));
}
