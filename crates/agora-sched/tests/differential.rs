//! Differential suite for `agora_sched::compat::run` (SPEC section 8, differential.rs). Each fixture under
//! tests/fixtures/ordering was produced by work.py (Python 3.14.6) and is replayed here without a Python process.
//! A case is `<name>.input.json` (argv words after "work", an optional tasks document, `compare_stderr`) and
//! `<name>.expected.json` (exit_code, stdout, stderr). The tasks document is served from the `read` closure under the
//! virtual path TASKS_PATH; nothing is written to disk.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use agora_sched::claims::{digest, workpy_digest};
use agora_sched::compat::{CompatOutput, is_given_tasks, run};
use serde_json::Value;

/// Virtual path of a fixture's tasks document. The `read` closure serves this path and nothing else.
const TASKS_PATH: &str = "fixture-tasks.json";
/// Number of fixtures the differential suite is specified to cover.
const FIXTURE_COUNT: usize = 57;
/// Upper bound on the external regeneration check.
const REGENERATE_TIMEOUT: Duration = Duration::from_secs(120);

struct Case {
    name: String,
    /// argv after "work", without the `--tasks` pair that replays the tasks document.
    args: Vec<String>,
    /// JSON text of the tasks document, or None when the case has no --tasks file.
    tasks: Option<String>,
    compare_stderr: bool,
    expected: CompatOutput,
}

fn words(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ordering")
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).expect("fixture file is readable");
    serde_json::from_str(&text).expect("fixture file is JSON")
}

fn load_case(dir: &Path, name: &str) -> Case {
    let input = read_json(&dir.join(format!("{name}.input.json")));
    let expected = read_json(&dir.join(format!("{name}.expected.json")));
    let args = input["args"]
        .as_array()
        .expect("args is an array")
        .iter()
        .map(|word| word.as_str().expect("args holds strings").to_owned())
        .collect();
    let tasks = (!input["tasks"].is_null()).then(|| input["tasks"].to_string());
    let exit_code = expected["exit_code"]
        .as_i64()
        .expect("exit_code is an integer");
    Case {
        name: name.to_owned(),
        args,
        tasks,
        compare_stderr: input["compare_stderr"] != Value::Bool(false),
        expected: CompatOutput {
            exit_code: i32::try_from(exit_code).expect("exit_code fits in i32"),
            stdout: expected["stdout"]
                .as_str()
                .expect("stdout is a string")
                .to_owned(),
            stderr: expected["stderr"]
                .as_str()
                .expect("stderr is a string")
                .to_owned(),
        },
    }
}

/// Every fixture, sorted by name, so failure reports are stable.
fn load_cases() -> Vec<Case> {
    let dir = fixture_dir();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("fixtures directory is readable")
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
    names.iter().map(|name| load_case(&dir, name)).collect()
}

fn run_case(case: &Case) -> CompatOutput {
    let mut args = case.args.clone();
    if case.tasks.is_some() {
        args.push("--tasks".to_owned());
        args.push(TASKS_PATH.to_owned());
    }
    let tasks = case.tasks.as_deref();
    run(&args, &mut |path: &str| match tasks {
        Some(text) if path == TASKS_PATH => Ok(text.to_owned()),
        _ => Err(format!("[Errno 2] No such file or directory: '{path}'")),
    })
}

/// The first line where `expected` and `actual` differ, with both texts, or None when they are equal.
fn first_difference(expected: &str, actual: &str) -> Option<String> {
    if expected == actual {
        return None;
    }
    let expected_lines: Vec<&str> = expected.split_inclusive('\n').collect();
    let actual_lines: Vec<&str> = actual.split_inclusive('\n').collect();
    let span = expected_lines.len().max(actual_lines.len());
    let Some(index) = (0..span).find(|&i| expected_lines.get(i) != actual_lines.get(i)) else {
        return Some("texts differ only in line structure".to_owned());
    };
    Some(format!(
        "line {}: got {:?}, expected {:?}",
        index + 1,
        actual_lines
            .get(index)
            .copied()
            .unwrap_or("<end of output>"),
        expected_lines
            .get(index)
            .copied()
            .unwrap_or("<end of output>"),
    ))
}

/// Every compared field of `got` that differs from the fixture, each with its first differing line.
fn mismatches(case: &Case, got: &CompatOutput) -> Vec<String> {
    let mut found = Vec::new();
    if got.exit_code != case.expected.exit_code {
        found.push(format!(
            "exit_code: got {}, expected {}",
            got.exit_code, case.expected.exit_code
        ));
    }
    if let Some(line) = first_difference(&case.expected.stdout, &got.stdout) {
        found.push(format!("stdout {line}"));
    }
    if case.compare_stderr
        && let Some(line) = first_difference(&case.expected.stderr, &got.stderr)
    {
        found.push(format!("stderr {line}"));
    }
    found
}

#[test]
fn all_fixtures_match_work_py() {
    let cases = load_cases();
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|case| {
            let problems = mismatches(case, &run_case(case));
            (!problems.is_empty()).then(|| format!("{}: {}", case.name, problems.join("; ")))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} fixtures differ from work.py:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn fixture_count_is_57() {
    assert_eq!(load_cases().len(), FIXTURE_COUNT);
}

#[test]
fn every_fixture_is_routed_as_given_tasks() {
    let unrouted: Vec<String> = load_cases()
        .iter()
        .filter_map(|case| {
            let mut args = case.args.clone();
            args.extend(["--tasks".to_owned(), TASKS_PATH.to_owned()]);
            (!is_given_tasks(&args)).then(|| case.name.clone())
        })
        .collect();
    assert!(
        unrouted.is_empty(),
        "fixtures not routed to compat::run: {unrouted:?}"
    );
}

#[test]
fn database_queue_commands_are_not_routed_as_given_tasks() {
    for args in [
        words(&["order"]),
        words(&["plan", "--agents", "2"]),
        words(&["order", "--scope", "x"]),
    ] {
        assert!(!is_given_tasks(&args), "{args:?} must use the queue");
    }
}

#[test]
fn running_the_fixture_set_twice_gives_identical_outputs() {
    let cases = load_cases();
    let first: Vec<CompatOutput> = cases.iter().map(run_case).collect();
    let second: Vec<CompatOutput> = cases.iter().map(run_case).collect();
    let changed: Vec<&str> = cases
        .iter()
        .zip(first.iter().zip(second.iter()))
        .filter(|(_, (before, after))| before != after)
        .map(|(case, _)| case.name.as_str())
        .collect();
    assert!(
        changed.is_empty(),
        "outputs changed between runs: {changed:?}"
    );
}

/// Re-runs gen_ordering.py against the committed fixtures. It needs python3 and the external work.py source tree,
/// so it is ignored by default; run it with `cargo test -p agora-sched --test differential -- --ignored`.
#[test]
#[ignore = "needs python3 and the external work.py source tree; run with --ignored"]
fn fixtures_still_match_current_work_py_when_regenerated() {
    let python_present = Command::new("python3")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !python_present {
        eprintln!("python3 not found; regeneration check skipped");
        return;
    }
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gen_ordering.py");
    let child = Command::new("python3")
        .arg(script)
        .args(["--check", "--extra"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 starts");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        // The receiver may have given up after the timeout; a closed channel is not an error here.
        let _ = sender.send(child.wait_with_output());
    });
    let output = receiver
        .recv_timeout(REGENERATE_TIMEOUT)
        .expect("gen_ordering.py finishes within the timeout")
        .expect("gen_ordering.py output is readable");
    assert_eq!(
        output.status.code(),
        Some(0),
        "fixtures differ from work.py:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn workpy_digest_matches_the_python_vector() {
    assert_eq!(
        workpy_digest(&words(&["touch", "/tmp/x"]), "/tmp/proj"),
        "2a76c0cd4983dc2de985784b48d45899087d92722178e600ef7649d8ba05201f"
    );
}

#[test]
fn d5_digest_changes_when_the_file_list_changes() {
    let argv = words(&["touch", "/tmp/x"]);
    let with_x = digest(&argv, "/tmp/proj", &words(&["/tmp/x"]));
    let with_y = digest(&argv, "/tmp/proj", &words(&["/tmp/y"]));
    let without_files = digest(&argv, "/tmp/proj", &[]);
    assert_ne!(with_x, with_y);
    assert_ne!(with_x, without_files);
}
