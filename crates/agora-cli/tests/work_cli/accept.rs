//! Acceptance of queued commands: a command runs only after its claimant accepts its digest, and an author cannot
//! accept its own command by default.

use super::*;

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The digest `work show` prints on its `digest: <hex>` line.
fn digest_of_show(shown: &str) -> String {
    let digest = shown
        .lines()
        .find_map(|line| line.strip_prefix("digest: "))
        .expect("show prints a digest line")
        .trim()
        .to_owned();
    assert!(is_hex64(&digest), "not a SHA-256 hex digest: {digest:?}");
    digest
}

#[test]
fn queued_command_cannot_run_before_its_claimant_accepts_it() {
    let env = Env::new();
    env.bootstrap();
    let made = env.folders.path().join("made-by-t1");
    let made_arg = made.to_str().expect("utf-8 path");
    env.work("alice", &["add", "--id", "t1", "--", "touch", made_arg])
        .success();
    env.work("bob", &["claim", "t1"]).success();
    env.work("bob", &["show", "t1"]).success();

    let assert = env.work("bob", &["run", "t1"]).code(1);
    assert!(
        stderr_of(&assert).contains("you have not accepted it"),
        "{}",
        stderr_of(&assert)
    );
    assert!(!made.exists(), "a command ran before its acceptance");
}

#[test]
fn queued_command_runs_after_the_claimant_accepts_its_digest() {
    let env = Env::new();
    env.bootstrap();
    let made = env.folders.path().join("made-by-t1");
    let made_arg = made.to_str().expect("utf-8 path");
    env.work("alice", &["add", "--id", "t1", "--", "touch", made_arg])
        .success();
    env.work("bob", &["claim", "t1"]).success();
    let digest = digest_of_show(&stdout_of(&env.work("bob", &["show", "t1"]).success()));

    env.work("bob", &["accept", "t1", "--digest", &digest])
        .success();
    let ran = env.work("bob", &["run", "t1"]).code(0);
    assert!(
        stdout_of(&ran).contains("job t1 finished with status 0"),
        "{}",
        stdout_of(&ran)
    );
    assert!(
        made.is_file(),
        "the accepted command did not create its file"
    );
}

#[test]
fn accept_with_a_digest_that_does_not_match_is_refused_and_nothing_is_accepted() {
    let env = Env::new();
    env.bootstrap();
    let made = env.folders.path().join("made-by-t1");
    let made_arg = made.to_str().expect("utf-8 path");
    env.work("alice", &["add", "--id", "t1", "--", "touch", made_arg])
        .success();
    env.work("bob", &["claim", "t1"]).success();

    let wrong = "0".repeat(64);
    let refused = env
        .work("bob", &["accept", "t1", "--digest", &wrong])
        .code(1);
    assert!(
        stderr_of(&refused).contains("the digest does not match"),
        "{}",
        stderr_of(&refused)
    );
    env.work("bob", &["run", "t1"]).code(1);
    assert!(
        !made.exists(),
        "a refused acceptance still let the command run"
    );
}

#[test]
fn author_cannot_accept_its_own_command_by_default() {
    let env = Env::new();
    env.bootstrap();
    let made = env.folders.path().join("made-by-t2");
    let made_arg = made.to_str().expect("utf-8 path");
    env.work("alice", &["add", "--id", "t2", "--", "touch", made_arg])
        .success();
    env.work("alice", &["claim", "t2"]).success();
    let digest = digest_of_show(&stdout_of(&env.work("alice", &["show", "t2"]).success()));

    let refused = env
        .work("alice", &["accept", "t2", "--digest", &digest])
        .code(1);
    assert!(
        stderr_of(&refused)
            .contains("a command must be accepted by an identity other than its author"),
        "{}",
        stderr_of(&refused)
    );
}

#[test]
fn accept_without_a_digest_is_a_clap_usage_error_exit_2() {
    let env = Env::new();
    let assert = env.work("alice", &["accept", "t1"]).code(2);
    assert!(
        stderr_of(&assert).contains("--digest"),
        "{}",
        stderr_of(&assert)
    );
}
