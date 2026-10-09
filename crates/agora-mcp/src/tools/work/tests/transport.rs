//! Refusals of the local-only calls over HTTP (D16): the local work, add and accept forms that stdio alone may run.

use super::*;

#[test]
fn over_http_local_work_is_refused_and_stdio_is_not() {
    assert_eq!(
        refusal(local_only(false, "running a command")),
        "running a command is not available over HTTP"
    );
    assert_eq!(local_only(true, "running a command"), Ok(()));
}

#[test]
fn add_over_http_refuses_a_command_a_working_directory_or_files() {
    let command = WorkAddArgs {
        command: Some(vec!["true".into()]),
        ..WorkAddArgs::default()
    };
    assert_eq!(
        refusal(add_local_only(&command, false)),
        "a command in work_add is not available over HTTP"
    );
    let cwd = WorkAddArgs {
        cwd: Some("/tmp".into()),
        ..WorkAddArgs::default()
    };
    assert_eq!(
        refusal(add_local_only(&cwd, false)),
        "cwd and files in work_add is not available over HTTP"
    );
    let files = WorkAddArgs {
        files: Some(vec!["src/a.rs".into()]),
        ..WorkAddArgs::default()
    };
    assert_eq!(
        refusal(add_local_only(&files, false)),
        "cwd and files in work_add is not available over HTTP"
    );
}

#[test]
fn add_over_http_allows_a_plain_task_and_an_empty_file_list() {
    let plain = WorkAddArgs {
        title: Some("x".into()),
        ..WorkAddArgs::default()
    };
    assert_eq!(add_local_only(&plain, false), Ok(()));
    let empty = WorkAddArgs {
        files: Some(Vec::new()),
        ..WorkAddArgs::default()
    };
    assert_eq!(add_local_only(&empty, false), Ok(()));
}

#[test]
fn add_on_stdio_allows_a_command_a_directory_and_files() {
    let a = WorkAddArgs {
        command: Some(vec!["true".into()]),
        cwd: Some("/tmp".into()),
        files: Some(vec!["a".into()]),
        ..WorkAddArgs::default()
    };
    assert_eq!(add_local_only(&a, true), Ok(()));
}

#[test]
fn accept_over_http_refuses_only_a_task_with_a_command() {
    assert_eq!(
        refusal(accept_local_only(true, false)),
        "accepting a command is not available over HTTP"
    );
    assert_eq!(accept_local_only(false, false), Ok(()));
    assert_eq!(accept_local_only(true, true), Ok(()));
}
