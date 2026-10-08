//! Unit tests for the given-tasks emulation; the fixture loop reads tests/fixtures/ordering.

use super::*;
use std::path::Path;

/// Stands in for the fixture's --tasks file; the closure serves its JSON for this path only.
const FIXTURE_FILE: &str = "<fixture tasks>";

fn words(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Runs `items` with a reader that must never be called.
fn run_words(items: &[&str]) -> CompatOutput {
    run(&words(items), &mut |path: &str| {
        Err(format!("unexpected read of {path}"))
    })
}

fn read_json(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).expect("fixture file");
    serde_json::from_str(&text).expect("fixture JSON")
}

/// Runs one differential fixture; returns why it differs, or None when every compared field matches.
fn fixture_difference(dir: &Path, name: &str) -> Option<String> {
    let input = read_json(&dir.join(format!("{name}.input.json")));
    let expected = read_json(&dir.join(format!("{name}.expected.json")));
    let mut args: Vec<String> = input["args"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|word| word.as_str().map(str::to_owned))
        .collect();
    let tasks = (!input["tasks"].is_null()).then(|| input["tasks"].to_string());
    if tasks.is_some() {
        args.push("--tasks".to_owned());
        args.push(FIXTURE_FILE.to_owned());
    }
    let mut read = |path: &str| -> Result<String, String> {
        match &tasks {
            Some(text) if path == FIXTURE_FILE => Ok(text.clone()),
            _ => Err(format!("[Errno 2] No such file or directory: '{path}'")),
        }
    };
    let got = run(&args, &mut read);
    let mut problems = Vec::new();
    if Some(i64::from(got.exit_code)) != expected["exit_code"].as_i64() {
        problems.push(format!(
            "exit {} vs {}",
            got.exit_code, expected["exit_code"]
        ));
    }
    if Some(got.stdout.as_str()) != expected["stdout"].as_str() {
        problems.push(format!("stdout {:?} vs {}", got.stdout, expected["stdout"]));
    }
    let compare_stderr = input["compare_stderr"] != serde_json::Value::Bool(false);
    if compare_stderr && Some(got.stderr.as_str()) != expected["stderr"].as_str() {
        problems.push(format!("stderr {:?} vs {}", got.stderr, expected["stderr"]));
    }
    (!problems.is_empty()).then(|| problems.join("; "))
}

#[test]
fn fixtures_match_python_work_byte_for_byte() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ordering");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("fixtures directory")
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_suffix(".input.json")
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    assert_eq!(names.len(), 57, "fixture count");
    let failures: Vec<String> = names
        .iter()
        .filter_map(|name| fixture_difference(&dir, name).map(|why| format!("{name}: {why}")))
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} fixtures differ:\n{}",
        failures.len(),
        names.len(),
        failures.join("\n")
    );
}

#[test]
fn order_with_one_task_prints_a_numbered_line() {
    let out = run_words(&["order", "--task", "id=a"]);
    assert_eq!(
        out,
        CompatOutput {
            exit_code: 0,
            stdout: "1. a (other)\n".into(),
            stderr: String::new()
        }
    );
}

#[test]
fn plan_with_two_agents_prints_lanes_and_bound() {
    let out = run_words(&[
        "plan",
        "--agents",
        "2",
        "--task",
        "id=a,size=2",
        "--task",
        "id=b",
    ]);
    assert_eq!(out.exit_code, 0);
    assert_eq!(
        out.stdout,
        "agent 1: a [0-2]\nagent 2: b [0-1]\nmakespan 2; lower bound 2\n"
    );
}

#[test]
fn json_output_names_the_policy_and_the_order() {
    let out = run_words(&["order", "--json", "--task", "id=a"]);
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.starts_with("{\n  \"policy\": \"fifo\""),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("\"id\": \"a\""), "{}", out.stdout);
}

#[test]
fn scope_with_given_tasks_is_refused_even_when_empty() {
    let out = run_words(&["order", "--scope", "", "--task", "id=a"]);
    assert_eq!(out.exit_code, 2);
    assert_eq!(out.stdout, "");
    assert_eq!(
        out.stderr,
        "work: --scope does not apply to tasks given with --tasks or --task\n"
    );
}

#[test]
fn conflict_cost_without_assign_is_refused() {
    let out = run_words(&[
        "plan",
        "--agents",
        "2",
        "--conflict-cost",
        "1",
        "--task",
        "id=a",
    ]);
    assert_eq!(out.exit_code, 2);
    assert_eq!(out.stderr, "work: --conflict-cost needs --assign\n");
}

#[test]
fn standard_input_may_be_named_once() {
    let out = run_words(&["order", "--tasks", "-", "--tasks", "-"]);
    assert_eq!(out.exit_code, 2);
    assert_eq!(
        out.stderr,
        "work: --tasks - reads the standard input once\n"
    );
}

#[test]
fn policy_accepts_the_equals_form() {
    let tasks = ["--task", "id=slow,size=5", "--task", "id=fast,size=1"];
    let mut argv = vec!["order", "--policy=wsjf"];
    argv.extend(tasks);
    assert_eq!(
        run_words(&argv).stdout,
        "1. fast (other)\n2. slow (other)\n"
    );
}

#[test]
fn policy_accepts_a_unique_abbreviation() {
    let tasks = ["--task", "id=slow,size=5", "--task", "id=fast,size=1"];
    let mut argv = vec!["order", "--pol", "wsjf"];
    argv.extend(tasks);
    assert_eq!(
        run_words(&argv).stdout,
        "1. fast (other)\n2. slow (other)\n"
    );
}

#[test]
fn an_ambiguous_abbreviation_is_an_error() {
    let out = run_words(&["order", "--ta", "id=a"]);
    assert_eq!(out.exit_code, 2);
    assert_eq!(out.stdout, "");
    assert!(
        out.stderr
            .contains("ambiguous option: --ta could match --tasks, --task"),
        "{}",
        out.stderr
    );
}

#[test]
fn a_command_after_double_dash_is_refused() {
    let out = run_words(&["order", "--task", "id=a", "--", "echo"]);
    assert_eq!(out.exit_code, 2);
    assert_eq!(
        out.stderr,
        "work: only add and run take a command after --\n"
    );
}

#[test]
fn agents_must_be_a_whole_number_of_at_least_one() {
    let text = run_words(&["plan", "--agents", "x", "--task", "id=a"]);
    assert_eq!(text.exit_code, 2);
    assert!(
        text.stderr
            .contains("argument --agents: 'x' is not a whole number"),
        "{}",
        text.stderr
    );
    let zero = run_words(&["plan", "--agents", "0", "--task", "id=a"]);
    assert_eq!(zero.exit_code, 2);
    assert!(
        zero.stderr.contains("'0' is less than 1"),
        "{}",
        zero.stderr
    );
}

#[test]
fn agents_stop_at_max_agents() {
    let over = run_words(&["plan", "--agents", "257", "--task", "id=a"]);
    assert_eq!(over.exit_code, 2);
    assert_eq!(over.stdout, "");
    assert!(
        over.stderr
            .contains("argument --agents: '257' is more than 256"),
        "{}",
        over.stderr
    );
    let at_cap = run_words(&["plan", "--agents", "256", "--task", "id=a"]);
    assert_eq!(at_cap.exit_code, 0, "{}", at_cap.stderr);
}

#[test]
fn plan_requires_agents_and_order_rejects_it() {
    let missing = run_words(&["plan", "--task", "id=a"]);
    assert_eq!(missing.exit_code, 2);
    assert!(missing.stderr.contains("--agents"), "{}", missing.stderr);
    let unknown = run_words(&["order", "--agents", "2", "--task", "id=a"]);
    assert_eq!(unknown.exit_code, 2);
    assert_eq!(unknown.stdout, "");
}

#[test]
fn conflict_cost_must_be_a_finite_number_of_zero_or_more() {
    for (value, detail) in [
        ("nan", "finite number, zero or more"),
        ("-1", "finite number"),
        ("x", "is not a number"),
    ] {
        let out = run_words(&[
            "plan",
            "--agents",
            "2",
            "--assign",
            "--conflict-cost",
            value,
            "--task",
            "id=a",
        ]);
        assert_eq!(out.exit_code, 2, "{value}");
        assert!(out.stderr.contains(detail), "{value}: {}", out.stderr);
    }
}

#[test]
fn unknown_options_and_bad_policies_exit_2() {
    let unknown = run_words(&["order", "--nope", "--task", "id=a"]);
    assert_eq!((unknown.exit_code, unknown.stdout.as_str()), (2, ""));
    let policy = run_words(&["order", "--policy", "x", "--task", "id=a"]);
    assert_eq!(policy.exit_code, 2);
    assert!(
        policy.stderr.contains("invalid choice: 'x'"),
        "{}",
        policy.stderr
    );
}

#[test]
fn an_empty_document_gives_no_tasks() {
    let out = run(&words(&["order", "--tasks", "-"]), &mut |_: &str| {
        Ok("[]".to_owned())
    });
    assert_eq!(
        out,
        CompatOutput {
            exit_code: 2,
            stdout: String::new(),
            stderr: "work: no tasks were given\n".into()
        }
    );
}

/// A JSON document of `count` tasks with distinct ids and no files, so they never conflict.
fn task_document(count: usize) -> String {
    let tasks: Vec<serde_json::Value> = (0..count)
        .map(|i| serde_json::json!({ "id": format!("t{i}") }))
        .collect();
    serde_json::Value::Array(tasks).to_string()
}

#[test]
fn more_than_2000_tasks_are_refused_with_exit_2() {
    let out = run(&words(&["order", "--tasks", "-"]), &mut |_: &str| {
        Ok(task_document(2001))
    });
    assert_eq!(
        out,
        CompatOutput {
            exit_code: 2,
            stdout: String::new(),
            stderr: "work: more than 2000 tasks\n".into()
        }
    );
}

#[test]
fn exactly_2000_tasks_are_accepted() {
    let out = run(&words(&["order", "--tasks", "-"]), &mut |_: &str| {
        Ok(task_document(2000))
    });
    assert_eq!(out.exit_code, 0, "{}", out.stderr);
}

#[test]
fn a_read_failure_becomes_the_message() {
    let message = "[Errno 2] No such file or directory: '/missing.json'";
    let out = run(
        &words(&["order", "--tasks", "/missing.json"]),
        &mut |_: &str| Err(message.to_owned()),
    );
    assert_eq!(out.stderr, format!("work: {message}\n"));
    assert_eq!(out.exit_code, 2);
}

#[test]
fn a_json_syntax_error_is_reported_with_its_position() {
    let out = run(&words(&["order", "--tasks", "-"]), &mut |_: &str| {
        Ok("{".to_owned())
    });
    assert_eq!(out.exit_code, 2);
    assert!(
        out.stderr
            .starts_with("work: Expecting value: line 1 column "),
        "{}",
        out.stderr
    );
}

#[test]
fn is_given_tasks_recognises_long_forms_and_abbreviations() {
    for yes in [
        &["order", "--task", "id=a"][..],
        &["plan", "--tasks=x.json"],
        &["order", "--tas"],
        &["order", "--ta"],
    ] {
        assert!(is_given_tasks(&words(yes)), "{yes:?}");
    }
    for no in [
        &["order", "--json"][..],
        &["order", "--t"],
        &["order", "--pol", "wsjf"],
        &["plan", "--agents", "2"],
    ] {
        assert!(!is_given_tasks(&words(no)), "{no:?}");
    }
}
