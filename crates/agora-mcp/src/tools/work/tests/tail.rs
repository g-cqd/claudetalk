//! The output of a finished wait: where the job tail goes on stdio and over HTTP, and which task receives it.

use super::*;

/// A finished pid wait whose log tail holds one line, as the daemon returns it.
fn pid_wait_with_tail() -> WaitResult {
    serde_json::from_value(json!({
        "status": 0,
        "condition": "pid",
        "seconds": 1.0,
        "tail": ["secret job output"],
    }))
    .expect("a wait result")
}

#[test]
fn over_http_the_tail_is_left_out_of_the_json_and_the_text() {
    let mut result = pid_wait_with_tail();
    let (structured, text) = wait_output(&mut result, false, false).expect("output");
    assert!(structured.get("tail").is_none(), "{structured}");
    assert_eq!(structured["condition"], json!("pid"));
    assert!(!text.contains("secret job output"), "{text}");
    assert!(text.contains("every process exited"), "{text}");
}

#[test]
fn on_stdio_the_tail_is_in_the_json_and_the_text_and_the_text_is_framed() {
    let mut result = pid_wait_with_tail();
    let (structured, text) = wait_output(&mut result, false, true).expect("output");
    assert_eq!(structured["tail"], json!(["secret job output"]));
    assert!(text.contains("secret job output"), "{text}");
    let out = framed_parts(structured, &text);
    assert_eq!(out.structured["notice"], UNTRUSTED_DATA_NOTICE);
    assert!(out.text.starts_with(UNTRUSTED_DATA_NOTICE), "{}", out.text);
    assert!(out.text.contains("<untrusted-data>"), "{}", out.text);
    assert!(out.text.contains("secret job output"), "{}", out.text);
}

/// A task record in `state`; `started` gives it a start time, as a task that ran has.
fn task_in(id: &str, state: &str, started: bool) -> TaskRecord {
    let mut value = json!({
        "seq": 1, "id": id, "title": "", "kind": "other", "tier": 2,
        "author": "aa", "author_name": "ada", "state": state,
        "size": 1.0, "value": 1.0, "weight": 1.0, "queued_ms": 0
    });
    if started {
        value["started_ms"] = json!(5);
    }
    serde_json::from_value(value).expect("a task record")
}

/// A finished task wait over `tasks`, carrying `tail` as the daemon returns it.
fn task_wait(tasks: Vec<TaskRecord>, timed_out: bool, tail: &[&str]) -> WaitResult {
    serde_json::from_value(json!({
        "status": 0,
        "timed_out": timed_out,
        "condition": "tasks",
        "seconds": 1.0,
        "tail": tail,
        "tasks": tasks,
    }))
    .expect("a wait result")
}

#[test]
fn a_task_wait_shows_the_tail_inside_the_frame_on_stdio() {
    let tasks = vec![task_in("t1", "queued", false), task_in("t2", "done", true)];
    let mut result = task_wait(tasks, false, &["hello-from-job"]);
    let (structured, text) = wait_output(&mut result, true, true).expect("output");
    assert_eq!(structured["tail"], json!(["hello-from-job"]));
    let out = framed_parts(structured, &text);
    let open = out
        .text
        .find("<untrusted-data>")
        .expect("the text is framed");
    let close = out
        .text
        .find("</untrusted-data>")
        .expect("the frame closes");
    let line = out.text.find("hello-from-job").expect("the tail is shown");
    assert!(
        open < line && line < close,
        "tail outside the frame: {}",
        out.text
    );
}

#[test]
fn over_http_a_task_wait_leaves_the_tail_out_of_the_text() {
    let tasks = vec![task_in("t2", "done", true)];
    let mut result = task_wait(tasks, false, &["hello-from-job"]);
    let (structured, text) = wait_output(&mut result, true, false).expect("output");
    assert!(structured.get("tail").is_none(), "{structured}");
    assert!(!text.contains("hello-from-job"), "{text}");
    assert!(text.contains("work: "), "the status line stays: {text}");
}

#[test]
fn a_finished_wait_gives_the_tail_to_the_first_final_task() {
    let tasks = vec![task_in("t1", "running", true), task_in("t2", "done", true)];
    let tail = vec!["out".to_owned()];
    assert_eq!(
        tails_of(&tasks, &tail, false),
        vec![Vec::<String>::new(), vec!["out".to_owned()]]
    );
}

#[test]
fn a_timed_out_wait_gives_the_tail_to_the_first_started_task() {
    let tasks = vec![
        task_in("t1", "claimed", false),
        task_in("t2", "running", true),
    ];
    let tail = vec!["out".to_owned()];
    assert_eq!(
        tails_of(&tasks, &tail, true),
        vec![Vec::<String>::new(), vec!["out".to_owned()]]
    );
}

#[test]
fn no_tail_gives_every_task_no_lines() {
    let tasks = vec![task_in("t1", "done", true), task_in("t2", "done", true)];
    assert_eq!(tails_of(&tasks, &[], false), vec![Vec::<String>::new(); 2]);
}
