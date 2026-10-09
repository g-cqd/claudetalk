//! Golden rows: work.py's signatures for each row, one test per family of argv shapes.

use super::{PLACE, sig, words};
use agora_sched::signature::signatures;

/// One row of work.py's signature output for `argv` in `PLACE`.
struct Golden {
    argv: &'static [&'static str],
    full: &'static str,
    head: &'static str,
    prog: &'static str,
    tool: &'static str,
}

/// The rows whose signature differs from work.py's, one line each; empty when every row agrees.
fn disagreements(rows: &[Golden]) -> Vec<String> {
    rows.iter()
        .filter_map(|row| {
            let got = signatures(&words(row.argv), PLACE);
            let want = sig(row.full, row.head, row.prog, row.tool);
            (got != want).then(|| format!("{:?}: got {got:?}, work.py gives {want:?}", row.argv))
        })
        .collect()
}

fn assert_matches_work_py(rows: &[Golden]) {
    let bad = disagreements(rows);
    assert!(
        bad.is_empty(),
        "signatures differ from work.py:\n{}",
        bad.join("\n")
    );
}

// Golden rows, all in PLACE. Quirks are work.py's own: `time nice -n 10 make` reads `10` as the program, and a
// `timeout` duration becomes the program in the same way.

#[rustfmt::skip]
const PLAIN: &[Golden] = &[
    Golden { argv: &["swift", "build", "--package-path", "/Users/gc/app/Packages/Core", "-c", "release", "--jobs", "4", "--scratch-path", "/tmp/build-1234"], full: "a720fab3b30d2559d1797772f868db17", head: "app/Packages/Core :: swift build --jobs --package-path --scratch-path -c jobs=4 config=release", prog: "app/Packages/Core :: swift build", tool: "swift build" },
    Golden { argv: &["make", "test"], full: "f76578a893ca2af00eee47961eeb62b1", head: "app/Packages/Core :: make test", prog: "app/Packages/Core :: make test", tool: "make test" },
    Golden { argv: &["bash", "-lc", "make test"], full: "76a01683c7c1344a72a513684539c321", head: "app/Packages/Core :: bash -lc", prog: "app/Packages/Core :: bash", tool: "bash" },
];

#[rustfmt::skip]
const SHELL_SCRIPTS: &[Golden] = &[
    Golden { argv: &["sh", "-c", "make test"], full: "f76578a893ca2af00eee47961eeb62b1", head: "app/Packages/Core :: make test", prog: "app/Packages/Core :: make test", tool: "make test" },
    Golden { argv: &["sh", "-c", "cd Packages && export X=1; env FOO=2 swift test --parallel 2>&1 > out.log"], full: "945f190badc5f2c03acf50028c8c65cb", head: "app/Packages/Core :: swift test --parallel", prog: "app/Packages/Core :: swift test", tool: "swift test" },
    Golden { argv: &["sh", "-c", "make build 2>&1 | tee log.txt"], full: "42ffca088c79cbe86d646b27932b47cc", head: "app/Packages/Core :: make build", prog: "app/Packages/Core :: make build", tool: "make build" },
    Golden { argv: &["sh", "-c", "npm ci && npm test > out.txt 2>&1"], full: "8a7fef18d4dc3cd6d75e1bbcc30cad69", head: "app/Packages/Core :: npm ci", prog: "app/Packages/Core :: npm ci", tool: "npm ci" },
    Golden { argv: &["sh", "-c", "false || echo fallback; exit 3"], full: "c7b67d93da2e31f2c800555586b13a11", head: "app/Packages/Core :: false", prog: "app/Packages/Core :: false", tool: "false" },
    Golden { argv: &["sh", "-c", "echo \"a b\" 'c d' | wc -l"], full: "a7203c1fa17d39cd291a5535389d9b84", head: "app/Packages/Core :: wc -l", prog: "app/Packages/Core :: wc", tool: "wc" },
    Golden { argv: &["sh", "-c", "cd /tmp/x && ./run-7.sh 2>/dev/null; echo done"], full: "96cb55674e7a15f4d6527e1d82d664f2", head: "app/Packages/Core :: run-#.sh", prog: "app/Packages/Core :: run-#.sh", tool: "run-#.sh" },
    Golden { argv: &["sh", "-c", "(cd a; make) > log 2>&1 &"], full: "986f5205a089053a1202f419fba7a72b", head: "app/Packages/Core :: make", prog: "app/Packages/Core :: make", tool: "make" },
    Golden { argv: &["sh", "-c", "FOO=1 BAR=2 make"], full: "cb06bba502f14763c5c6b6c5425cd855", head: "app/Packages/Core :: make", prog: "app/Packages/Core :: make", tool: "make" },
    Golden { argv: &["sh", "-c", "exit 0"], full: "192350a1d84b5f8e1e85e3130ccae5eb", head: "app/Packages/Core :: exit", prog: "app/Packages/Core :: exit", tool: "exit" },
];

#[rustfmt::skip]
const WRAPPERS: &[Golden] = &[
    Golden { argv: &["env", "FOO=1", "BAR=two", "make", "-j", "8"], full: "5891dc9dbfd0c0d0c63c1dc330df9e82", head: "app/Packages/Core :: make -j jobs=8", prog: "app/Packages/Core :: make", tool: "make" },
    Golden { argv: &["time", "nice", "-n", "10", "make"], full: "233fdde1a821ed3f20ebeeac33c85040", head: "app/Packages/Core :: 10 make", prog: "app/Packages/Core :: 10 make", tool: "10 make" },
    Golden { argv: &["timeout", "30", "python3", "script.py", "--n", "5"], full: "8552466e7d1822f512cea92c0ef4dd5b", head: "app/Packages/Core :: script.py --n", prog: "app/Packages/Core :: script.py", tool: "script.py" },
    Golden { argv: &["timeout", "-s", "KILL", "60", "./run-123.sh"], full: "d47bfbe1876e477b69068d49c898fbe1", head: "app/Packages/Core :: 60", prog: "app/Packages/Core :: 60", tool: "60" },
    Golden { argv: &["nice", "timeout", "-k", "5", "60", "make", "-j", "8"], full: "9c35189de6dccb2b1e0a06ef8b90714c", head: "app/Packages/Core :: 60 make -j jobs=8", prog: "app/Packages/Core :: 60 make", tool: "60 make" },
];

#[rustfmt::skip]
const RUN_IDS: &[Golden] = &[
    Golden { argv: &["./run-123.sh", "--id", "3f2a9c1d7e", "/a/b/out-2024.log"], full: "4dc2bee15d158a523dca9b423e2bdd27", head: "app/Packages/Core :: run-#.sh --id", prog: "app/Packages/Core :: run-#.sh", tool: "run-#.sh" },
    Golden { argv: &["./run-456.sh", "--id", "0b1c2d3e4f5a", "/a/b/out-2025.log"], full: "4dc2bee15d158a523dca9b423e2bdd27", head: "app/Packages/Core :: run-#.sh --id", prog: "app/Packages/Core :: run-#.sh", tool: "run-#.sh" },
    Golden { argv: &["cli", "run", "123e4567-e89b-12d3-a456-426614174000", "--n=42"], full: "c39c4a23e77cbb69b01d0e711efa99da", head: "app/Packages/Core :: cli run --n", prog: "app/Packages/Core :: cli run", tool: "cli run" },
];

#[rustfmt::skip]
const FLAGS: &[Golden] = &[
    Golden { argv: &["make", "--jobs=16"], full: "0436459b3e5214df2d6cc542e700af4c", head: "app/Packages/Core :: make --jobs", prog: "app/Packages/Core :: make", tool: "make" },
    Golden { argv: &["cmake", "--build", "build", "--config", "release", "-j", "4"], full: "6c2dff92ce664b81c8374c84eb4e65ad", head: "app/Packages/Core :: cmake build --build --config -j jobs=4", prog: "app/Packages/Core :: cmake build", tool: "cmake build" },
    Golden { argv: &["make", "-j", "x1", "--jobs", "007"], full: "4cfb8bcf97a5a401cd595372dfbf6e7c", head: "app/Packages/Core :: make x1 --jobs -j", prog: "app/Packages/Core :: make x1", tool: "make x1" },
    Golden { argv: &["cargo", "test", "-p", "agora-sched", "--", "--nocapture"], full: "10bf6006a47868ee6dd70e24c2b929b3", head: "app/Packages/Core :: cargo test -- --nocapture -p", prog: "app/Packages/Core :: cargo test", tool: "cargo test" },
];

#[rustfmt::skip]
const INTERPRETERS: &[Golden] = &[
    Golden { argv: &["bash", "-c", "ls -la /tmp/x"], full: "93c5a09aa08f0628d0d01f080e3fb268", head: "app/Packages/Core :: ls -la", prog: "app/Packages/Core :: ls", tool: "ls" },
    Golden { argv: &["python3", "script.py", "--verbose"], full: "fdf3dcaab3532d3c3fa6c2f3f4a81ca9", head: "app/Packages/Core :: script.py --verbose", prog: "app/Packages/Core :: script.py", tool: "script.py" },
    Golden { argv: &["python3", "/x/y/tool.py", "--n", "5"], full: "ccf4360f2c64de6b16e39e54175ddc3d", head: "app/Packages/Core :: tool.py --n", prog: "app/Packages/Core :: tool.py", tool: "tool.py" },
    Golden { argv: &["./build.sh", "-c", "release"], full: "1291da0c989ce19c39f889c1d88d6ec6", head: "app/Packages/Core :: build.sh release -c config=release", prog: "app/Packages/Core :: build.sh release", tool: "build.sh release" },
    Golden { argv: &["node", "server.js", "--port", "8080"], full: "372f293ef2b88f416d975a77e92817c8", head: "app/Packages/Core :: server.js --port", prog: "app/Packages/Core :: server.js", tool: "server.js" },
    Golden { argv: &["perl", "-e", "print 1"], full: "0af9d7a078173cfec7fa611059ad65af", head: "app/Packages/Core :: perl -e", prog: "app/Packages/Core :: perl", tool: "perl" },
];

#[rustfmt::skip]
const GLUED_PATHS: &[Golden] = &[
    Golden { argv: &["cmd", "-I/x/y", "-c", "debug"], full: "7c9c26af6802901b7bb63248ebb5ca94", head: "app/Packages/Core :: cmd debug -I<p:y~62b0ea> -c config=debug", prog: "app/Packages/Core :: cmd debug", tool: "cmd debug" },
    Golden { argv: &["swiftc", "-I~/inc/dir", "-c", "release"], full: "866877879931a58c671c9580a4e1023e", head: "app/Packages/Core :: swiftc release -I~<p:dir~6ef51b> -c config=release", prog: "app/Packages/Core :: swiftc release", tool: "swiftc release" },
];

#[rustfmt::skip]
const TOOLS: &[Golden] = &[
    Golden { argv: &["xcrun", "-sdk", "macosx", "swiftc", "-emit-executable"], full: "e539c3f0a9b104c65eefda0758549b9f", head: "app/Packages/Core :: -sdk macosx -emit-executable", prog: "app/Packages/Core :: -sdk macosx", tool: "-sdk macosx" },
    Golden { argv: &["git", "-C", "/Users/gc/proj-12/sub3", "status", "--short"], full: "49d4e38c797b62a4ea7975733483d2d3", head: "app/Packages/Core :: git status --short -C", prog: "app/Packages/Core :: git status", tool: "git status" },
];

#[rustfmt::skip]
const MALFORMED: &[Golden] = &[
    Golden { argv: &["sh", "-c", "echo \"unterminated"], full: "e8aa04800490873ae4a3927661c027d8", head: "app/Packages/Core :: echo", prog: "app/Packages/Core :: echo", tool: "echo" },
    Golden { argv: &[], full: "bbcbaa85614aad558b976f7ccba78a30", head: "app/Packages/Core :: ", prog: "app/Packages/Core :: ", tool: "" },
    Golden { argv: &["sh", "-c", ""], full: "bbcbaa85614aad558b976f7ccba78a30", head: "app/Packages/Core :: ", prog: "app/Packages/Core :: ", tool: "" },
];

#[test]
fn plain_commands_and_bash_lc_match_work_py() {
    assert_matches_work_py(PLAIN);
}

#[test]
fn sh_c_scripts_split_into_simple_commands_and_drop_redirections() {
    assert_matches_work_py(SHELL_SCRIPTS);
}

#[test]
fn wrapper_words_are_skipped_before_the_program_and_counts_read_as_programs() {
    assert_matches_work_py(WRAPPERS);
}

#[test]
fn ids_and_uuids_are_masked_so_reruns_share_one_signature() {
    assert_matches_work_py(RUN_IDS);
}

#[test]
fn only_a_plain_jobs_count_and_a_release_or_debug_configuration_are_kept() {
    assert_matches_work_py(FLAGS);
}

#[test]
fn interpreter_scripts_and_script_paths_are_reduced_to_their_names() {
    assert_matches_work_py(INTERPRETERS);
}

#[test]
fn a_glued_path_flag_keeps_its_prefix_and_a_digest_of_its_path() {
    assert_matches_work_py(GLUED_PATHS);
}

#[test]
fn xcrun_and_git_keep_their_options_in_the_head() {
    assert_matches_work_py(TOOLS);
}

#[test]
fn an_unbalanced_quote_falls_back_to_whitespace_words_and_empty_argv_is_the_empty_signature() {
    assert_matches_work_py(MALFORMED);
}

#[test]
fn a_long_flag_list_is_compacted_to_255_characters_with_a_digest_of_the_whole() {
    let mut argv = vec!["make".to_string()];
    argv.extend((0..40).map(|i| format!("--flag-{i:03}")));
    let s = signatures(&argv, PLACE);
    let prefix: String = std::iter::once("app/Packages/Core :: make".to_string())
        .chain((0..=16).map(|i| format!(" --flag-{i:03}")))
        .collect();
    assert_eq!(s.head.chars().count(), 255);
    assert_eq!(
        s.head,
        format!("{prefix} --flag-01~49052706746b0a1c256a521b7e080c3a")
    );
    assert_eq!(s.full, "c01b66a7cff38ac576717a320f860917");
    assert_eq!(s.prog, "app/Packages/Core :: make");
    assert_eq!(s.tool, "make");
}

#[test]
fn a_single_300_character_word_is_compacted_in_its_prog_and_its_tool() {
    let s = signatures(&["x".repeat(300)], PLACE);
    let x201 = "x".repeat(201);
    let x222 = "x".repeat(222);
    assert_eq!(s.full, "46a9e73fbb7a371a32926da1ff079413");
    assert_eq!(
        s.prog,
        format!("{PLACE} :: {x201}~46a9e73fbb7a371a32926da1ff079413")
    );
    assert_eq!(s.tool, format!("{x222}~0d4e2ca9e9cbced7a7a5380eb29e1a37"));
}
