//! The given-tasks plan (`work_plan` with tasks or a task list) and the agent-count limits.

use super::*;

#[test]
fn plan_agents_accepts_one_to_256_only() {
    assert_eq!(plan_agents(1).expect("one agent"), 1);
    assert_eq!(plan_agents(256).expect("256 agents"), 256);
    assert_eq!(refusal(plan_agents(0)), "agents must be between 1 and 256");
    assert_eq!(
        refusal(plan_agents(257)),
        "agents must be between 1 and 256"
    );
    assert_eq!(
        refusal(plan_agents(u32::MAX)),
        "agents must be between 1 and 256"
    );
}

#[test]
fn given_tasks_plan_matches_the_differential_fixture() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../agora-sched/tests/fixtures/ordering/plan_two_agents_basic.input.json");
    let input: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture input")).expect("json");
    let expected_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../agora-sched/tests/fixtures/ordering/plan_two_agents_basic.expected.json");
    let expected: Value =
        serde_json::from_str(&std::fs::read_to_string(expected_path).expect("fixture output"))
            .expect("json");
    let tasks = input["tasks"].as_array().expect("tasks array");
    let out = given_tasks_plan(tasks, 2, OrderPolicy::Fifo, false, None, None).expect("a plan");
    let stdout: Value =
        serde_json::from_str(expected["stdout"].as_str().expect("stdout")).expect("json");
    assert_eq!(out.json["plan"], stdout);
}

#[test]
fn given_tasks_plan_refuses_an_unknown_prerequisite_with_the_compat_message() {
    let tasks = vec![json!({"id": "a", "after": ["ghost"]})];
    let message = refusal(given_tasks_plan(
        &tasks,
        1,
        OrderPolicy::Fifo,
        false,
        None,
        None,
    ));
    assert_eq!(message, "task 'a' is after unknown task 'ghost'");
}

#[test]
fn given_tasks_plan_text_is_the_plan_as_a_person_reads_it() {
    let tasks = vec![json!({"id": "a", "size": 2.0}), json!({"id": "b"})];
    let out = given_tasks_plan(&tasks, 2, OrderPolicy::Fifo, false, None, None).expect("a plan");
    assert_eq!(
        out.text,
        "agent 1: a [0-2]\nagent 2: b [0-1]\nmakespan 2; lower bound 2"
    );
    assert_eq!(out.json["text"], json!(out.text));
    assert!(out.json["plan"].is_object(), "the plan is the JSON run");
}

#[test]
fn given_tasks_plan_refusal_is_the_same_for_the_text_run() {
    let tasks = vec![json!({"id": "a", "after": ["ghost"]})];
    let message = refusal(given_tasks_plan(
        &tasks,
        1,
        OrderPolicy::Wsjf,
        true,
        Some(2.0),
        None,
    ));
    assert_eq!(message, "task 'a' is after unknown task 'ghost'");
}

#[test]
fn given_tasks_plan_refuses_a_scope() {
    let tasks = vec![json!({"id": "a"})];
    let message = refusal(given_tasks_plan(
        &tasks,
        1,
        OrderPolicy::Fifo,
        false,
        None,
        Some("proj"),
    ));
    assert_eq!(
        message,
        "--scope does not apply to tasks given with --tasks or --task"
    );
}
