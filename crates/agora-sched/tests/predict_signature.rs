//! Command signatures, directory placement and history predictions through the public API (ref-admission sections 1
//! and 2; scenario tables F = P1-P14 and I = G1-G9). P8 (status 126/127 excluded) and P13 (a running job's estimate)
//! are the store's history query and daemon's `estimated`, not this crate's API, so they are not covered here.
//!
//! The signature and placement goldens are work.py's own output, pasted as literals. Each signature row was printed by
//!   python3 -I -c 'import sys, json; sys.path.insert(0, "/Users/gc/Developer/skills/skills/planner/scripts");
//!                  import work; work.place = lambda cwd: "app/Packages/Core";
//!                  print(json.dumps(work.signatures(ARGV, None)))'
//! with ARGV the row's argv. The placement goldens run work.place(cwd) with os.path.isdir, os.path.isfile and open
//! replaced by a fake tree, and work.SCRATCH set to the four roots this crate is given. The prediction expectations
//! follow ref-admission section 15 F; the reference derived them by hand, so they were not run through work.py.

use agora_sched::predict::{History, Level, Prediction, Run, percentile, predict};
use agora_sched::signature::{PlaceProbe, Signatures, place, signatures};

const PLACE: &str = "app/Packages/Core";
const OTHER: &str = "other/Repo";

fn words(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|s| (*s).to_string()).collect()
}

fn sig(full: &str, head: &str, prog: &str, tool: &str) -> Signatures {
    Signatures {
        full: full.to_string(),
        head: head.to_string(),
        prog: prog.to_string(),
        tool: tool.to_string(),
    }
}

fn scratch() -> Vec<String> {
    [
        "/tmp",
        "/private/tmp",
        "/var/folders",
        "/private/var/folders",
    ]
    .iter()
    .map(|root| root.to_string())
    .collect()
}

/// A fake tree for `place`: directories, files with their content, and files that exist but cannot be read.
#[derive(Default)]
struct Tree {
    dirs: Vec<&'static str>,
    files: Vec<(&'static str, &'static str)>,
    unreadable: Vec<&'static str>,
}

impl PlaceProbe for Tree {
    fn is_dir(&self, p: &str) -> bool {
        self.dirs.contains(&p)
    }
    fn is_file(&self, p: &str) -> bool {
        self.files.iter().any(|(f, _)| *f == p) || self.unreadable.contains(&p)
    }
    fn read_head(&self, p: &str, n: usize) -> Option<String> {
        if self.unreadable.contains(&p) {
            return None;
        }
        self.files
            .iter()
            .find(|(f, _)| *f == p)
            .map(|(_, content)| content.chars().take(n).collect())
    }
}

fn place_in(cwd: &str, tree: &Tree) -> String {
    place(Some(cwd), tree, &scratch())
}

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

// Combined flows: a fake tree decides the place, and the place feeds the signature. Goldens from work.py with the
// same tree (work.place real, work.SCRATCH fixed, work.signatures(argv, cwd)).

#[test]
fn a_linked_worktree_and_its_main_checkout_share_one_signature() {
    let worktree = Tree {
        dirs: vec!["/r/main/.git"],
        files: vec![("/r/wt/.git", "gitdir: /r/main/.git/worktrees/wt\n")],
        ..Tree::default()
    };
    let main = Tree {
        dirs: vec!["/r/main/.git"],
        ..Tree::default()
    };
    let argv = words(&["swift", "test", "--parallel"]);
    let from_worktree = signatures(&argv, &place_in("/r/wt/Packages/Core", &worktree));
    let from_main = signatures(&argv, &place_in("/r/main/Packages/Core", &main));
    assert_eq!(from_worktree, from_main);
    assert_eq!(
        from_worktree,
        sig(
            "70d61d9baa1f2f42bb7e7f1e418fa93a",
            "main/Packages/Core :: swift test --parallel",
            "main/Packages/Core :: swift test",
            "swift test",
        )
    );
}

#[test]
fn a_submodule_is_signed_under_its_own_directory_name() {
    let submodule = Tree {
        files: vec![(
            "/r/outer/libs/core/.git",
            "gitdir: /r/outer/.git/modules/core\n",
        )],
        ..Tree::default()
    };
    let placed = place_in("/r/outer/libs/core/src", &submodule);
    assert_eq!(placed, "core/src");
    assert_eq!(
        signatures(&words(&["make", "test"]), &placed),
        sig(
            "fe910414fc880059063e57d95351f4b4",
            "core/src :: make test",
            "core/src :: make test",
            "make test",
        )
    );
}

#[test]
fn scratch_plain_and_checkout_directories_are_signed_with_their_digits_masked() {
    let none = Tree::default();
    let checkout = Tree {
        dirs: vec!["/Users/me/proj-12/.git"],
        ..Tree::default()
    };
    let argv = words(&["make", "test"]);
    let scratch_sig = signatures(&argv, &place_in("/tmp/build-99/x", &none));
    assert_eq!(
        scratch_sig,
        sig(
            "a7b1580f8c4cb650137ccf428c63c9b7",
            "tmp :: make test",
            "tmp :: make test",
            "make test"
        )
    );
    let plain_sig = signatures(&argv, &place_in("/Users/me/build-2024", &none));
    assert_eq!(
        plain_sig,
        sig(
            "02f82165c6ea469584094e0166a8d9ab",
            "build-# :: make test",
            "build-# :: make test",
            "make test"
        )
    );
    let digits_sig = signatures(&argv, &place_in("/Users/me/proj-12/sub3", &checkout));
    assert_eq!(
        digits_sig,
        sig(
            "3cb1e308b451f40a80dd2801bcf1cbb2",
            "proj-#/sub# :: make test",
            "proj-#/sub# :: make test",
            "make test"
        )
    );
}

// Placement alone (work.place with the fake tree, work.SCRATCH fixed).

#[test]
fn a_main_checkout_is_named_by_its_root_and_the_path_inside_it() {
    let tree = Tree {
        dirs: vec!["/Users/me/app/.git", "/Users/me/proj-12/.git"],
        ..Tree::default()
    };
    assert_eq!(
        place_in("/Users/me/app/Packages/Core", &tree),
        "app/Packages/Core"
    );
    assert_eq!(place_in("/Users/me/app", &tree), "app");
    assert_eq!(place_in("/Users/me/proj-12/sub3", &tree), "proj-#/sub#");
}

#[test]
fn a_trailing_slash_does_not_change_the_place() {
    let tree = Tree {
        dirs: vec!["/r/main/.git"],
        ..Tree::default()
    };
    assert_eq!(place_in("/r/main/sub/", &tree), "main/sub");
}

#[test]
fn a_linked_worktree_is_named_by_its_main_checkout() {
    let tree = Tree {
        files: vec![("/r/wt/.git", "gitdir: /r/main/.git/worktrees/wt\n")],
        ..Tree::default()
    };
    assert_eq!(place_in("/r/wt/Packages/Core", &tree), "main/Packages/Core");
    assert_eq!(place_in("/r/wt", &tree), "main");
}

#[test]
fn a_gitdir_line_ignores_padding_and_crlf_and_masks_digits_in_the_main_name() {
    let padded = Tree {
        files: vec![("/r/wt/.git", "gitdir:   /r/main/.git/worktrees/wt   \n")],
        ..Tree::default()
    };
    let crlf = Tree {
        files: vec![("/r/wt/.git", "gitdir: /r/main/.git/worktrees/wt\r\n")],
        ..Tree::default()
    };
    let digits = Tree {
        files: vec![("/r/wt/.git", "gitdir: /r/main-2/.git/worktrees/wt\n")],
        ..Tree::default()
    };
    assert_eq!(place_in("/r/wt/x", &padded), "main/x");
    assert_eq!(place_in("/r/wt/x", &crlf), "main/x");
    assert_eq!(place_in("/r/wt/x", &digits), "main-#/x");
}

#[test]
fn an_unreadable_or_odd_git_file_names_the_checkout_by_its_directory() {
    let unreadable = Tree {
        unreadable: vec!["/r/sub/.git"],
        ..Tree::default()
    };
    assert_eq!(place_in("/r/sub/src", &unreadable), "sub/src");
    for content in [
        "\ngitdir: /r/main/.git/worktrees/wt\n",
        "gitdir: /.git/worktrees/wt\n",
        "",
    ] {
        let tree = Tree {
            files: vec![("/r/wt/.git", content)],
            ..Tree::default()
        };
        assert_eq!(place_in("/r/wt/x", &tree), "wt/x", "content {content:?}");
    }
}

#[test]
fn a_scratch_directory_is_tmp_only_on_a_path_boundary() {
    let none = Tree::default();
    assert_eq!(place_in("/tmp/build-99/x", &none), "tmp");
    assert_eq!(place_in("/tmp", &none), "tmp");
    assert_eq!(place_in("/private/var/folders/ab/T/x", &none), "tmp");
    assert_eq!(place_in("/tmpx/y", &none), "y");
}

#[test]
fn a_plain_directory_is_its_own_name_with_digits_masked() {
    let none = Tree::default();
    assert_eq!(place_in("/Users/me/build-2024", &none), "build-#");
    assert_eq!(place_in("/Users/me/proj-12/sub3", &none), "sub#");
}

#[test]
fn no_directory_or_an_empty_one_is_none() {
    let none = Tree::default();
    assert_eq!(place(None, &none, &scratch()), "none");
    assert_eq!(place(Some(""), &none, &scratch()), "none");
}

// Predictions (ref-admission section 2 and 15 F). History is built through the signatures, the way the store
// pre-fetches it: per level, the newest runs whose signature at that level equals the query's.

/// A finished run of `argv` in `place`.
#[derive(Clone)]
struct Past {
    argv: &'static [&'static str],
    place: &'static str,
    run: Run,
}

/// A run of `secs` seconds from time 0 that used `cores` CPUs on average, with the given peak.
fn run(secs: i64, cores: f64, peak: Option<u64>) -> Run {
    Run {
        started_ms: 0,
        ended_ms: secs * 1000,
        cpu_seconds: secs as f64 * cores,
        rss_peak_kib: peak,
    }
}

fn seconds(run: &Run) -> f64 {
    (run.ended_ms - run.started_ms) as f64 / 1000.0
}

fn key(sig: &Signatures, level: usize) -> &str {
    match level {
        0 => &sig.full,
        1 => &sig.head,
        2 => &sig.prog,
        _ => &sig.tool,
    }
}

/// The history a prediction for `argv` in `place` reads. `runs` are in the order they ended, oldest first. Per level
/// the history holds the newest five runs whose signature at that level equals the query's; the durations of every
/// run are kept for the recent-run median.
fn history_for(runs: &[Past], argv: &[&str], place: &str) -> History {
    let query = signatures(&words(argv), place);
    let mut history = History::default();
    for past in runs.iter().rev() {
        let theirs = signatures(&words(past.argv), past.place);
        history.recent_durations_s.push(seconds(&past.run));
        for (level, bucket) in history.by_level.iter_mut().enumerate() {
            if key(&theirs, level) == key(&query, level) && bucket.len() < 5 {
                bucket.push(past.run.clone());
            }
        }
    }
    history
}

/// A history with runs only at the full level, newest first.
fn full_only(runs: Vec<Run>) -> History {
    History {
        by_level: [runs, Vec::new(), Vec::new(), Vec::new()],
        ..History::default()
    }
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn no_history_predicts_the_declared_weight_for_a_thirty_second_run() {
    assert_eq!(
        predict(&History::default(), 3.0, 8.0),
        Prediction {
            cores: 3.0,
            rss_kib: None,
            rss_p90_kib: None,
            duration_s: 30.0,
            duration_p90_s: None,
            level: Level::None,
            runs: 0,
        }
    );
    assert_eq!(
        predict(&History::default(), 3.0, 8.0).basis(),
        "no history: the declared weight"
    );
}

#[test]
fn without_history_the_duration_is_the_median_of_recent_runs_of_any_command() {
    let h = History {
        recent_durations_s: vec![90.0, 10.0, 20.0],
        ..History::default()
    };
    let p = predict(&h, 3.0, 8.0);
    assert_eq!(p.level, Level::None);
    assert_eq!(p.duration_s, 20.0);
    assert_eq!(p.duration_p90_s, None);
}

#[test]
fn the_full_level_reads_only_the_five_newest_runs() {
    // Newest first; the sixth, oldest run would move the median, the peak and the p90 if it were read.
    let h = full_only(vec![
        run(60, 1.0, Some(400_000)),
        run(80, 1.5, Some(900_000)),
        run(50, 0.8, Some(300_000)),
        run(70, 3.0, Some(500_000)),
        run(65, 1.2, Some(450_000)),
        run(500, 7.0, Some(9_000_000)),
    ]);
    let p = predict(&h, 7.5, 8.0);
    assert_eq!(p.level, Level::Full);
    assert_close(p.cores, 1.2);
    assert_eq!(p.rss_kib, Some(900_000));
    assert_eq!(p.rss_p90_kib, Some(900_000));
    assert_close(p.duration_s, 65.0);
    assert_eq!(p.duration_p90_s, Some(80.0));
    assert_eq!(p.runs, 5);
    assert_eq!(p.basis(), "5 runs of the same command");
}

#[test]
fn cores_are_capped_at_the_declared_weight() {
    let p = predict(&full_only(vec![run(10, 6.0, None)]), 2.0, 8.0);
    assert_eq!(p.level, Level::Full);
    assert_close(p.cores, 2.0);
}

#[test]
fn cores_are_floored_at_a_tenth_of_a_core() {
    let p = predict(&full_only(vec![run(10, 0.0, None)]), 8.0, 8.0);
    assert_close(p.cores, 0.1);
}

#[test]
fn a_run_that_ends_no_later_than_it_starts_lasts_a_twentieth_of_a_second() {
    let zero = Run {
        started_ms: 5_000,
        ended_ms: 5_000,
        cpu_seconds: 0.05,
        rss_peak_kib: None,
    };
    let backwards = Run {
        started_ms: 5_000,
        ended_ms: 1_000,
        cpu_seconds: 0.01,
        rss_peak_kib: None,
    };
    let p = predict(&full_only(vec![zero]), 8.0, 8.0);
    assert_close(p.duration_s, 0.05);
    assert_close(p.cores, 1.0);
    let q = predict(&full_only(vec![backwards]), 8.0, 8.0);
    assert_close(q.duration_s, 0.05);
    assert_close(q.cores, 0.2);
}

#[test]
fn timestamps_at_the_ends_of_the_i64_range_do_not_overflow() {
    let span = Run {
        started_ms: i64::MIN,
        ended_ms: i64::MAX,
        cpu_seconds: 1.0,
        rss_peak_kib: None,
    };
    let p = predict(&full_only(vec![span]), 8.0, 8.0);
    assert_eq!(p.duration_s, i64::MAX as f64 / 1000.0);
    assert_close(p.cores, 0.1);
}

#[test]
fn an_even_count_of_runs_takes_the_mean_of_the_middle_two_cores() {
    let p = predict(
        &full_only(vec![run(10, 2.0, None), run(10, 1.0, None)]),
        8.0,
        8.0,
    );
    assert_eq!(p.level, Level::Full);
    assert_close(p.cores, 1.5);
}

#[test]
fn peaks_missing_from_some_runs_are_ignored_and_all_missing_gives_none() {
    let mixed = predict(
        &full_only(vec![
            run(10, 1.0, None),
            run(10, 1.0, Some(300)),
            run(10, 1.0, Some(100)),
        ]),
        8.0,
        8.0,
    );
    assert_eq!((mixed.rss_kib, mixed.rss_p90_kib), (Some(300), Some(300)));
    let none = predict(&full_only(vec![run(10, 1.0, None)]), 8.0, 8.0);
    assert_eq!((none.rss_kib, none.rss_p90_kib), (None, None));
}

#[test]
fn percentile_is_nearest_rank_so_up_to_five_values_give_the_maximum() {
    for n in 1..=5 {
        let xs: Vec<f64> = (1..=n).map(f64::from).collect();
        assert_eq!(percentile(&xs, 0.9), Some(f64::from(n)), "n = {n}");
    }
    let ten: Vec<f64> = (1..=10).map(f64::from).collect();
    assert_eq!(percentile(&ten, 0.9), Some(9.0));
}

#[test]
fn a_weight_below_a_tenth_of_a_core_is_not_raised_to_the_floor() {
    assert_close(predict(&History::default(), 0.05, 8.0).cores, 0.05);
    let busy = full_only(vec![run(10, 10.0, None)]);
    assert_close(predict(&busy, 0.05, 8.0).cores, 0.05);
}

#[test]
fn a_program_is_predicted_from_its_products_as_their_heads_fill() {
    let app = Past {
        argv: &["swift", "build", "--product", "App"],
        place: PLACE,
        run: run(100, 4.0, None),
    };
    let kit = Past {
        argv: &["swift", "build", "--product", "Kit"],
        place: PLACE,
        run: run(50, 2.0, None),
    };
    let verbose = Past {
        argv: &["swift", "build", "-v"],
        place: PLACE,
        run: run(30, 1.0, None),
    };
    let tool = ["swift", "build", "--product", "Tool"];

    let one = predict(
        &history_for(std::slice::from_ref(&app), &tool, PLACE),
        7.5,
        8.0,
    );
    assert_eq!(one.level, Level::None);

    let two = predict(
        &history_for(&[app.clone(), kit.clone()], &tool, PLACE),
        7.5,
        8.0,
    );
    assert_eq!(two.level, Level::Head);
    assert_close(two.cores, 3.0);
    assert_eq!(two.basis(), "2 runs of the same program and flags");

    let three = predict(
        &history_for(
            &[app, kit, verbose],
            &["swift", "build", "--verbose"],
            PLACE,
        ),
        7.5,
        8.0,
    );
    assert_eq!(three.level, Level::Prog);
    assert_eq!(three.runs, 3);
    assert_close(three.cores, 2.0);
    assert_eq!(three.basis(), "3 runs of the same program in this place");
}

#[test]
fn one_run_of_the_same_command_is_enough_for_the_full_level() {
    let past = [Past {
        argv: &["swift", "test", "--parallel"],
        place: PLACE,
        run: run(10, 2.0, Some(500)),
    }];
    let p = predict(
        &history_for(&past, &["swift", "test", "--parallel"], PLACE),
        7.5,
        8.0,
    );
    assert_eq!(
        p,
        Prediction {
            cores: 2.0,
            rss_kib: Some(500),
            rss_p90_kib: Some(500),
            duration_s: 10.0,
            duration_p90_s: Some(10.0),
            level: Level::Full,
            runs: 1,
        }
    );
    assert_eq!(p.basis(), "1 run of the same command");
}

#[test]
fn a_finer_level_with_one_run_beats_a_coarser_level_with_more() {
    let past = [
        Past {
            argv: &["swift", "test", "--parallel", "Foo"],
            place: PLACE,
            run: run(10, 4.0, None),
        },
        Past {
            argv: &["swift", "test", "--parallel"],
            place: PLACE,
            run: run(10, 2.0, None),
        },
    ];
    let p = predict(
        &history_for(&past, &["swift", "test", "--parallel"], PLACE),
        7.5,
        8.0,
    );
    assert_eq!(p.level, Level::Full);
    assert_eq!(p.runs, 1);
    assert_close(p.cores, 2.0);
}

#[test]
fn head_needs_two_runs_of_the_same_program_and_flags() {
    let mut past = vec![Past {
        argv: &["swift", "test", "--parallel", "Foo"],
        place: PLACE,
        run: run(10, 2.0, None),
    }];
    let query = ["swift", "test", "--parallel"];
    assert_eq!(
        predict(&history_for(&past, &query, PLACE), 7.5, 8.0),
        Prediction {
            cores: 7.5,
            rss_kib: None,
            rss_p90_kib: None,
            duration_s: 10.0,
            duration_p90_s: None,
            level: Level::None,
            runs: 0,
        }
    );
    past.push(Past {
        argv: &["swift", "test", "--parallel", "Bar"],
        place: PLACE,
        run: run(20, 6.0, None),
    });
    let p = predict(&history_for(&past, &query, PLACE), 7.5, 8.0);
    assert_eq!(p.level, Level::Head);
    assert_eq!(p.runs, 2);
    assert_close(p.cores, 4.0);
    assert_close(p.duration_s, 15.0);
    assert_eq!(p.duration_p90_s, Some(20.0));
    assert_eq!(p.basis(), "2 runs of the same program and flags");
}

#[test]
fn prog_needs_three_runs_of_the_program_in_the_same_place() {
    let mut past = vec![
        Past {
            argv: &["swift", "test", "--filter", "A"],
            place: PLACE,
            run: run(10, 1.0, None),
        },
        Past {
            argv: &["swift", "test", "--filter", "B"],
            place: PLACE,
            run: run(10, 1.0, None),
        },
    ];
    let query = ["swift", "test", "--parallel"];
    assert_eq!(
        predict(&history_for(&past, &query, PLACE), 7.5, 8.0).level,
        Level::None
    );
    past.push(Past {
        argv: &["swift", "test", "-v"],
        place: PLACE,
        run: run(10, 1.0, None),
    });
    let p = predict(&history_for(&past, &query, PLACE), 7.5, 8.0);
    assert_eq!(p.level, Level::Prog);
    assert_eq!(p.runs, 3);
    assert_eq!(p.basis(), "3 runs of the same program in this place");
}

#[test]
fn tool_needs_three_runs_of_the_program_in_any_place() {
    let mut past: Vec<Past> = (0..2)
        .map(|_| Past {
            argv: &["swift", "test", "--filter", "A"],
            place: OTHER,
            run: run(10, 1.0, None),
        })
        .collect();
    let query = ["swift", "test", "--parallel"];
    assert_eq!(
        predict(&history_for(&past, &query, PLACE), 7.5, 8.0).level,
        Level::None
    );
    past.push(Past {
        argv: &["swift", "test", "--filter", "B"],
        place: OTHER,
        run: run(10, 1.0, None),
    });
    let p = predict(&history_for(&past, &query, PLACE), 7.5, 8.0);
    assert_eq!(p.level, Level::Tool);
    assert_eq!(p.runs, 3);
    assert_eq!(p.basis(), "3 runs of the same program");
}

#[test]
fn exclusive_cold_start_predicts_one_core_capped_by_the_weight() {
    assert_eq!(
        predict(&History::default(), 8.0, 8.0),
        Prediction {
            cores: 1.0,
            rss_kib: None,
            rss_p90_kib: None,
            duration_s: 30.0,
            duration_p90_s: None,
            level: Level::Exclusive,
            runs: 0,
        }
    );
    assert_eq!(
        predict(&History::default(), 8.0, 8.0).basis(),
        "typical exclusive use; no history"
    );
}

#[test]
fn typical_exclusive_use_is_the_median_of_the_exclusive_runs() {
    let h = History {
        exclusive_runs: vec![run(10, 0.5, None), run(10, 1.2, None), run(10, 0.8, None)],
        ..History::default()
    };
    let p = predict(&h, 8.0, 8.0);
    assert_eq!(p.level, Level::Exclusive);
    assert_close(p.cores, 0.8);
}

#[test]
fn typical_exclusive_use_is_capped_by_the_weight_when_the_runs_used_more() {
    let mut runs = vec![run(10, 10.0, None); 20];
    runs.extend(vec![run(10, 0.2, None); 3]);
    let h = History {
        exclusive_runs: runs,
        ..History::default()
    };
    assert_close(predict(&h, 8.0, 8.0).cores, 8.0);
}

#[test]
fn fewer_than_three_exclusive_runs_predict_one_core() {
    let h = History {
        exclusive_runs: vec![run(10, 5.0, None), run(10, 5.0, None)],
        ..History::default()
    };
    assert_close(predict(&h, 8.0, 8.0).cores, 1.0);
}

#[test]
fn exclusive_needs_at_least_two_cpus_and_a_weight_of_all_of_them() {
    assert_eq!(
        predict(&History::default(), 2.0, 2.0).level,
        Level::Exclusive
    );
    assert_eq!(predict(&History::default(), 8.0, 1.0).level, Level::None);
    assert_eq!(predict(&History::default(), 3.5, 4.0).level, Level::None);
}
