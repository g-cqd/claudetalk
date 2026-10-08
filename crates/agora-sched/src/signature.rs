//! Command signatures for history lookups (ref-admission section 1). No regex crate: hand-written matchers.
//! `place` is in `signature/location.rs`, the command split in `signature/command.rs`, the token matchers and digests
//! in `signature/text.rs`, and the shlex port in `signature/lex.rs`.

use std::collections::BTreeSet;

mod command;
mod lex;
mod location;
mod text;

pub use location::place;

use command::{has_script_extension, program_part, simple_commands};
use text::{
    basename, collapse, compact, digest, flag_key, is_py_space, is_subcommand, listed,
    looks_like_path, normal_token, path_tag_inner, scrub,
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Signatures {
    pub full: String,
    pub head: String,
    pub prog: String,
    pub tool: String,
}

pub trait PlaceProbe {
    fn is_dir(&self, p: &str) -> bool;
    fn is_file(&self, p: &str) -> bool;
    fn read_head(&self, p: &str, n: usize) -> Option<String>;
}

const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash"];
const INTERPRETERS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "python", "python3", "perl", "ruby", "node",
];

const PRELUDE: &[&str] = &[
    "cd", "export", "set", "true", ":", "source", ".", "ulimit", "rm", "mkdir", "echo", "unset",
    "trap", "sleep", "cp", "mv", "ln", "test", "[", "wait", "exit", "printf",
];

fn first_word(words: &[String]) -> &str {
    words.first().map_or("", String::as_str)
}

/// shlex (posix, punctuation_chars ";&|()<>") port used by shell_tokens.
#[allow(clippy::result_unit_err)]
pub fn shlex_split_punct(script: &str) -> Result<Vec<String>, ()> {
    lex::split(script)
}

/// The argv, with the script of `sh -c SCRIPT` split into its own words and operators. Only the exact token `-c`
/// among the first two options counts, so `bash -lc` is not split.
pub fn shell_tokens(argv: &[String]) -> Vec<String> {
    let is_shell = argv
        .first()
        .is_some_and(|first| listed(SHELLS, basename(first)));
    let has_c = argv.iter().skip(1).take(2).any(|a| a.as_str() == "-c");
    if !(is_shell && has_c) {
        return argv.to_vec();
    }
    let script = argv
        .iter()
        .position(|a| a.as_str() == "-c")
        .and_then(|i| argv.get(i + 1))
        .map_or("", String::as_str);
    lex::split(script).unwrap_or_else(|()| {
        script
            .split(is_py_space)
            .filter(|word| !word.is_empty())
            .map(str::to_string)
            .collect()
    })
}

/// The four signatures of a command, most specific first (work.py `signatures`): `full` is a digest of every word,
/// `head` the program, subcommand, flags, `--jobs` and configuration, `prog` the program and subcommand with the
/// place, and `tool` the program and subcommand alone. `place` is the directory kind from `place`.
pub fn signatures(argv: &[String], place: &str) -> Signatures {
    let tokens = shell_tokens(argv);
    let full_text = tokens
        .iter()
        .map(|t| normal_token(t))
        .collect::<Vec<_>>()
        .join(" ");
    let commands: Vec<Vec<String>> = simple_commands(&tokens)
        .into_iter()
        .map(|command| program_part(&command).to_vec())
        .filter(|part| !part.is_empty())
        .collect();
    let mut main = commands
        .iter()
        .find(|part| {
            part.first()
                .is_some_and(|word| !listed(PRELUDE, basename(word)))
        })
        .or_else(|| commands.first())
        .cloned()
        .unwrap_or_else(|| vec![String::new()]);

    if listed(INTERPRETERS, basename(first_word(&main))) {
        let script = main
            .iter()
            .skip(1)
            .find(|t| !t.starts_with('-'))
            .filter(|s| !s.is_empty())
            .cloned();
        let index = script
            .filter(|s| looks_like_path(s) || has_script_extension(s))
            .and_then(|script| main.iter().position(|t| *t == script));
        if let Some(index) = index {
            main = main.get(index..).unwrap_or(&[]).to_vec();
        }
    }

    let head_word = first_word(&main);
    let program = if looks_like_path(head_word) {
        path_tag_inner(head_word)
    } else {
        scrub(head_word)
    };
    let sub_candidate = main
        .iter()
        .skip(1)
        .find(|t| !t.starts_with('-') && !looks_like_path(t))
        .map_or("", String::as_str);
    let sub = if is_subcommand(sub_candidate) {
        scrub(sub_candidate)
    } else {
        String::new()
    };
    let flags: BTreeSet<String> = main
        .iter()
        .skip(1)
        .filter(|t| t.starts_with('-'))
        .map(|t| flag_key(t.split_once('=').map_or(t.as_str(), |(flag, _)| flag)))
        .collect();
    let jobs = main.windows(2).find_map(|w| match w {
        [flag, value] if flag.as_str() == "--jobs" || flag.as_str() == "-j" => Some(value.as_str()),
        _ => None,
    });
    let jobs_part = jobs
        .filter(|j| !j.is_empty() && j.bytes().all(|b| b.is_ascii_digit()))
        .map(|j| format!("jobs={j}"));
    let config = main.windows(2).find_map(|w| match w {
        [flag, value] if flag.as_str() == "-c" || flag.as_str() == "--configuration" => {
            Some(value.as_str())
        }
        _ => None,
    });
    let config_part = config
        .filter(|c| *c == "debug" || *c == "release")
        .map(|c| format!("config={c}"));

    let mut words = vec![program.clone(), sub.clone()];
    words.extend(flags);
    words.extend(jobs_part);
    words.extend(config_part);
    let head = collapse(&words.join(" "));
    let tool = format!("{program} {sub}")
        .trim_end_matches(is_py_space)
        .to_string();
    Signatures {
        full: digest(&format!("{place} :: {full_text}"), 32),
        head: compact(&format!("{place} :: {head}")),
        prog: compact(&format!("{place} :: {tool}")),
        tool: compact(&tool),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn sig(items: &[&str], place: &str) -> Signatures {
        signatures(&argv(items), place)
    }

    fn words(script: &str) -> Vec<String> {
        shlex_split_punct(script).expect("the script lexes")
    }

    #[test]
    fn shlex_splits_operators_and_words() {
        assert_eq!(
            words("cd a && b; c | d"),
            ["cd", "a", "&&", "b", ";", "c", "|", "d"]
        );
        assert_eq!(
            words("x&>y z>>w a|&b;;c (d) {e} }"),
            [
                "x", "&>", "y", "z", ">>", "w", "a", "|&", "b", ";;", "c", "(", "d", ")", "{e}",
                "}",
            ]
        );
    }

    #[test]
    fn shlex_redirect_leaves_a_stray_fd_digit() {
        assert_eq!(
            words("x 2>&1 > out.log"),
            ["x", "2", ">&", "1", ">", "out.log"]
        );
    }

    #[test]
    fn shlex_quotes_and_escapes_join_words() {
        assert_eq!(words("echo \"a b\" 'c'"), ["echo", "a b", "c"]);
        assert_eq!(
            words("echo \"x\\\\y\" \"x\\zy\""),
            ["echo", "x\\y", "x\\zy"]
        );
        assert_eq!(words("a'b'c \"d\\\"e\" f\\ g"), ["abc", "d\"e", "f g"]);
    }

    #[test]
    fn shlex_unbalanced_quote_is_an_error() {
        assert!(shlex_split_punct("echo \"unterminated").is_err());
    }

    #[test]
    fn shell_tokens_split_only_an_exact_dash_c_script() {
        assert_eq!(
            shell_tokens(&argv(&["sh", "-c", "make test"])),
            ["make", "test"]
        );
        assert_eq!(
            shell_tokens(&argv(&["bash", "-lc", "make test"])),
            ["bash", "-lc", "make test"]
        );
        assert_eq!(shell_tokens(&argv(&["sh", "-c"])), Vec::<String>::new());
    }

    #[test]
    fn shell_tokens_fall_back_to_whitespace_split_on_an_unbalanced_quote() {
        assert_eq!(
            shell_tokens(&argv(&["sh", "-c", "echo \"unterminated"])),
            ["echo", "\"unterminated"]
        );
    }

    #[test]
    fn swift_build_example_has_the_signatures_of_work_py() {
        let s = sig(
            &[
                "swift",
                "build",
                "--package-path",
                "/Users/gc/app/Packages/Core",
                "-c",
                "release",
                "--jobs",
                "4",
                "--scratch-path",
                "/tmp/build-1234",
            ],
            "app/Packages/Core",
        );
        assert_eq!(s.full, "a720fab3b30d2559d1797772f868db17");
        assert_eq!(
            s.head,
            "app/Packages/Core :: swift build --jobs --package-path --scratch-path -c jobs=4 config=release"
        );
        assert_eq!(s.prog, "app/Packages/Core :: swift build");
        assert_eq!(s.tool, "swift build");
    }

    #[test]
    fn run_scripts_with_other_ids_and_numbers_share_their_signatures() {
        let a = sig(
            &["./run-12.sh", "--id", "3f2a9c1d7e", "/a/b/out-2024.log"],
            "x",
        );
        let b = sig(
            &["./run-99.sh", "--id", "0b1c2d3e4f5a", "/c/d/out-1999.log"],
            "x",
        );
        assert_eq!(a, b);
        assert_eq!(a.full, "19c54f1cf7a4473ef1a351f828c2d517");
        assert_eq!(a.head, "x :: run-#.sh --id");
        assert_eq!(a.tool, "run-#.sh");
    }

    #[test]
    fn sh_c_script_is_split_and_the_prelude_is_skipped() {
        let s = sig(
            &[
                "sh",
                "-c",
                "cd Packages && export X=1; env FOO=2 swift test --parallel 2>&1 > out.log",
            ],
            "app/Packages/Core",
        );
        assert_eq!(s.tool, "swift test");
        assert_eq!(s.head, "app/Packages/Core :: swift test --parallel");
        assert_eq!(s.full, "945f190badc5f2c03acf50028c8c65cb");
    }

    #[test]
    fn sh_c_and_the_direct_argv_share_full() {
        let script = sig(&["sh", "-c", "make test"], "p");
        let direct = sig(&["make", "test"], "p");
        assert_eq!(script.full, direct.full);
        assert_eq!(script.full, "058de8fe4c3b79abe5bcf1e036ce9aa6");
        assert_eq!(script.head, "p :: make test");
    }

    #[test]
    fn bash_lc_is_not_split() {
        let s = sig(&["bash", "-lc", "make test"], "p");
        assert_eq!(s.head, "p :: bash -lc");
        assert_eq!(s.prog, "p :: bash");
        assert_eq!(s.tool, "bash");
        assert_eq!(s.full, "30755cfd02e430e6c5420c7805d36572");
    }

    #[test]
    fn nice_count_is_read_as_the_program() {
        let s = sig(&["nice", "-n", "10", "make"], "p");
        assert_eq!(s.tool, "10 make");
        assert_eq!(s.head, "p :: 10 make");
    }

    #[test]
    fn long_head_and_prog_are_compacted_to_255_characters() {
        let mut items = vec!["make".to_string()];
        items.extend((0..40).map(|i| format!("--flag-{i:03}")));
        let s = signatures(&items, "p");
        assert_eq!(s.head.chars().count(), 255);
        assert!(s.head.starts_with("p :: make --flag-000 --flag-001"));
        assert!(s.head.ends_with("~ce1ce4b94707aa3943313df61118fc1a"));
        assert_eq!(s.prog, "p :: make");
        assert_eq!(s.full, "da783102c74405990ec6d4820db8c70c");

        let long = "x".repeat(300);
        let s = signatures(&[long], "p");
        assert_eq!(
            s.tool,
            format!("{}~0d4e2ca9e9cbced7a7a5380eb29e1a37", "x".repeat(222))
        );
        assert_eq!(
            s.prog,
            format!("p :: {}~d19fbd375db4bfc645432469cf58f76f", "x".repeat(217))
        );
        assert_eq!(s.full, "d19fbd375db4bfc645432469cf58f76f");
    }

    #[test]
    fn empty_argv_and_a_bare_sh_c_give_the_empty_signature() {
        let empty = signatures(&[], "p");
        assert_eq!(empty.full, "abad0d16871f1c848a562607e34c1fc4");
        assert_eq!(empty.head, "p :: ");
        assert_eq!(empty.prog, "p :: ");
        assert_eq!(empty.tool, "");
        assert_eq!(sig(&["sh", "-c"], "p"), empty);
    }

    #[test]
    fn an_interpreter_script_becomes_the_program() {
        let py = sig(&["python3", "/x/y/tool.py", "--n", "5"], "p");
        assert_eq!(py.head, "p :: tool.py --n");
        assert_eq!(py.prog, "p :: tool.py");
        assert_eq!(py.tool, "tool.py");
        assert_eq!(py.full, "41ab8557753049881425219163ef6688");
        assert_eq!(sig(&["node", "server.js"], "p").head, "p :: server.js");
    }

    #[test]
    fn jobs_and_configuration_are_kept_only_when_plain() {
        assert_eq!(
            sig(&["FOO=1", "BAR=2", "make", "-j", "8"], "p").head,
            "p :: make -j jobs=8"
        );
        assert_eq!(
            sig(&["make", "-j", "x1", "--jobs", "007"], "p").head,
            "p :: make x1 --jobs -j"
        );
        assert_eq!(
            sig(
                &[
                    "cmd",
                    "--configuration",
                    "Release",
                    "--configuration",
                    "release"
                ],
                "p"
            )
            .head,
            "p :: cmd --configuration"
        );
        assert_eq!(
            sig(&["swift", "test", "-j"], "p").head,
            "p :: swift test -j"
        );
    }

    #[test]
    fn a_glued_path_flag_and_a_uuid_argument_are_masked() {
        let glued = sig(&["cmd", "-I/x/y", "-c", "debug"], "p");
        assert_eq!(glued.head, "p :: cmd debug -I<p:y~62b0ea> -c config=debug");
        assert_eq!(glued.prog, "p :: cmd debug");
        assert_eq!(glued.full, "4802a666e96fc408610ed490814b408e");
        let uuid = sig(
            &[
                "cli",
                "run",
                "123e4567-e89b-12d3-a456-426614174000",
                "--n=42",
            ],
            "p",
        );
        assert_eq!(uuid.head, "p :: cli run --n");
        assert_eq!(uuid.full, "acbe37a816f50930e59ec05c4008f313");
    }

    #[test]
    fn xcrun_keeps_its_options_and_a_missing_cwd_is_none_in_the_head() {
        let xcrun = sig(&["xcrun", "-sdk", "macosx", "swiftc"], "p");
        assert_eq!(xcrun.head, "p :: -sdk macosx");
        assert_eq!(xcrun.tool, "-sdk macosx");
        assert_eq!(sig(&["make"], "none").head, "none :: make");
    }
}
