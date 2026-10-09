//! Checks that the docs name only `agora` subcommands and long flags the CLI has.
//!
//! The tree comes from `agora --help` and `agora <path> --help`, plus the `HIDDEN` subcommands.
//! Invocations come from code blocks tagged with no language, `sh`, `bash`, `shell`, `console`
//! or `zsh`, and from inline code spans of README.md, SECURITY.md, crates/agora-mcp/README.md,
//! docs/ and plugin/claude/. A subcommand word descends into it; a root word naming none fails.
//! Each `--long` flag must belong to the deepest command or to the root. A `$ ` or `% ` prompt,
//! `NAME=value` words, a double space (a table column) and `| ; ( ) && backtick` bound a command.
//! Opting out: a line containing `<!-- docs-check: skip -->` is not checked, nor the line after.

use assert_cmd::Command;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CMD_TIMEOUT: Duration = Duration::from_secs(30);
const SKIP: &str = "<!-- docs-check: skip -->";
/// Fence languages whose blocks are checked. An empty tag counts.
const SCANNED: [&str; 6] = ["", "sh", "bash", "shell", "console", "zsh"];
/// Subcommands the binary accepts but its help does not list. Each is probed with `--help`.
const HIDDEN: [&str; 1] = ["_supervise"];

/// A command's subcommands, and its long flags with whether each takes a value.
#[derive(Default)]
struct Node {
    children: BTreeSet<String>,
    flags: BTreeMap<String, bool>,
}

/// Every command by its path from the root. The root's flags are the global ones.
type Tree = BTreeMap<Vec<String>, Node>;

/// Walks the tree from the root, asking `help` for each path. `hidden` become root children.
fn build_tree(help: &mut dyn FnMut(&[String]) -> String, hidden: &[&str]) -> Tree {
    let mut tree = Tree::new();
    let mut queue = vec![Vec::<String>::new()];
    while let Some(path) = queue.pop() {
        let mut node = parse_help(&help(&path));
        if path.is_empty() {
            node.children.extend(hidden.iter().map(|h| h.to_string()));
        }
        for c in &node.children {
            queue.push([path.clone(), vec![c.clone()]].concat());
        }
        tree.insert(path, node);
    }
    tree
}

/// `[a-z][a-z0-9-]*`
fn is_word(s: &str) -> bool {
    s.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Reads clap-style help: the `Commands:` entries and the long flags of each `... options:` or
/// `... arguments:` section. Wrapped text is not an entry, and `help` is left out.
fn parse_help(text: &str) -> Node {
    let mut node = Node::default();
    let (mut commands, mut flags) = (false, false);
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if !line.starts_with(' ') {
            let heading = line.trim_end().to_ascii_lowercase();
            commands = heading == "commands:";
            flags = heading.ends_with("options:") || heading.ends_with("arguments:");
        } else if commands {
            // Entries are indented by two spaces; wrapped text by more.
            let entry = line.strip_prefix("  ").filter(|r| !r.starts_with(' '));
            let name = entry.and_then(|r| r.split(' ').next()).unwrap_or_default();
            if is_word(name) && name != "help" {
                node.children.insert(name.to_owned());
            }
        } else if flags && line.trim_start().starts_with('-') {
            // In `-j, --json  Print JSON` the spec ends at the first double space.
            let spec = line.trim_start().split("  ").next().unwrap_or_default();
            let takes_value = spec.contains('<');
            let long = spec.split([',', ' ']).filter_map(|w| w.strip_prefix("--"));
            node.flags.extend(
                long.filter(|n| is_word(n))
                    .map(|n| (n.to_owned(), takes_value)),
            );
        }
    }
    node
}

/// A shell word. A word that began with a quote is a value, never a name.
#[derive(Default)]
struct Token {
    text: String,
    quoted: bool,
}

impl Token {
    /// A subcommand name: a bare lowercase name, optionally with a leading `_`.
    fn word(&self) -> Option<&str> {
        let name = self.text.strip_prefix('_').unwrap_or(&self.text);
        (!self.quoted && is_word(name)).then_some(self.text.as_str())
    }

    /// The name of a `--long` flag, without `=value` or trailing punctuation.
    fn flag(&self) -> Option<&str> {
        let name = self.text.strip_prefix("--")?.split('=').next()?;
        (!self.quoted).then_some(name.trim_end_matches(['.', ',', ':', ';', ')']))
    }
}

/// Splits on whitespace. Quotes group words and are dropped; a word that starts with one is
/// `quoted`.
fn tokenize(line: &str) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    let (mut open, mut quote) = (false, None::<char>);
    for c in line.chars() {
        if quote.is_none() && c.is_whitespace() {
            open = false;
            continue;
        }
        if !open {
            out.push(Token::default());
            open = true;
        }
        let token = out.last_mut().expect("a token is open");
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (None, '\'' | '"') => {
                quote = Some(c);
                token.quoted |= token.text.is_empty();
            }
            _ => token.text.push(c),
        }
    }
    out
}

/// The language tag of a line that opens a backtick fence: "```sh" gives "sh".
fn fence_open(line: &str) -> Option<String> {
    let t = line.trim_start();
    let info = t.trim_start_matches('`');
    let lang = info.split_whitespace().next().unwrap_or_default();
    (t.len() - info.len() >= 3 && !info.contains('`')).then(|| lang.to_ascii_lowercase())
}

/// A line that closes a fence is made of backticks alone.
fn is_fence_close(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3 && t.bytes().all(|b| b == b'`')
}

/// Drops a `$ ` or `% ` prompt, then the `NAME=value` words in front of a command.
fn strip_noise(segment: &str) -> &str {
    let mut rest = segment.trim();
    if let Some(after) = rest.strip_prefix("$ ").or(rest.strip_prefix("% ")) {
        rest = after.trim_start();
    }
    while let Some((word, after)) = rest.split_once(char::is_whitespace) {
        let name = word.split_once('=').map_or("", |(name, _)| name);
        if name.is_empty() || !name.bytes().all(|b| b == b'_' || b.is_ascii_alphanumeric()) {
            break;
        }
        rest = after.trim_start();
    }
    rest
}

/// The errors in one invocation. `words[0]` is the program name and is not checked.
fn check_invocation(tree: &Tree, words: &[Token]) -> Vec<String> {
    let root = &tree[&Vec::<String>::new()];
    let args = &words[1..];
    let (mut path, mut ended, mut errors) = (Vec::<String>::new(), false, Vec::new());
    let mut rest = args.iter();
    while let Some(token) = rest.next() {
        if let Some(flag) = token.flag() {
            // A value-taking flag consumes the next word, which therefore names no subcommand.
            let takes_value = tree[&path].flags.get(flag).or(root.flags.get(flag));
            if takes_value == Some(&true) && !token.text.contains('=') {
                rest.next();
            }
        } else if !ended {
            match token.word() {
                Some(w) if tree[&path].children.contains(w) => path.push(w.to_owned()),
                Some(w) if path.is_empty() && w != "help" => {
                    errors.push(format!("unknown subcommand '{w}'"));
                    ended = true;
                }
                _ => ended = true,
            }
        }
    }

    let here = &tree[&path];
    let shown = format!("agora {}", path.join(" ")).trim_end().to_owned();
    for flag in args.iter().filter_map(Token::flag) {
        let known = here.flags.contains_key(flag) || root.flags.contains_key(flag);
        let error = format!("unknown flag '--{flag}' for '{shown}'");
        if is_word(flag) && !known && !errors.contains(&error) {
            errors.push(error);
        }
    }
    errors
}

/// Checks one document. Returns the invocations found and the failures, each rendered as
/// `file:line: command — message`.
fn check_document(file: &str, text: &str, tree: &Tree) -> (usize, Vec<String>) {
    let lines: Vec<&str> = text.lines().collect();
    let (mut invocations, mut failures) = (0, Vec::new());
    // Some(scanned) while inside a fence, where `scanned` says whether its language is checked.
    let mut fence: Option<bool> = None;
    for (idx, &line) in lines.iter().enumerate() {
        // A line inside a scanned fence, or the odd pieces of a backtick split (inline spans).
        let mut pieces: Vec<&str> = Vec::new();
        match fence {
            Some(_) if is_fence_close(line) => fence = None,
            Some(scanned) => pieces.extend(scanned.then_some(line)),
            None => match fence_open(line) {
                Some(lang) => fence = Some(SCANNED.contains(&lang.as_str())),
                None => pieces.extend(line.split('`').skip(1).step_by(2)),
            },
        }
        let skipped = lines[idx].contains(SKIP) || (idx > 0 && lines[idx - 1].contains(SKIP));
        for piece in pieces.into_iter().filter(|_| !skipped) {
            // Two spaces in a row end the command, which keeps a table's description column out.
            let cut = piece.split("  ").next().unwrap_or_default();
            let segments = cut
                .split(['|', ';', '(', ')', '`'])
                .flat_map(|s| s.split("&&"));
            for segment in segments {
                let words = tokenize(strip_noise(segment));
                // A bare `agora`, and prose such as "agora Daemon", name no command.
                let [first, second, ..] = words.as_slice() else {
                    continue;
                };
                let agora = !first.quoted && first.text.rsplit('/').next() == Some("agora");
                if !agora || second.text.starts_with(|c: char| c.is_ascii_uppercase()) {
                    continue;
                }
                invocations += 1;
                let shown = strip_noise(segment);
                for m in check_invocation(tree, &words) {
                    failures.push(format!("{file}:{}: {shown} — {m}", idx + 1));
                }
            }
        }
    }
    (invocations, failures)
}

/// Every `.md` file under `dir`, recursively. A missing directory has none.
fn md_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for path in entries.map(|e| e.expect("directory entry").path()) {
        if path.is_dir() {
            md_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            out.push(path);
        }
    }
}

/// The documents to check, as (path from the repository root, content), sorted. README.md must
/// exist; any other listed file that is missing is skipped.
fn documents(root: &Path) -> Vec<(String, String)> {
    assert!(root.join("README.md").is_file(), "README.md is missing");
    let fixed = ["README.md", "SECURITY.md", "crates/agora-mcp/README.md"];
    let mut paths: Vec<PathBuf> = fixed.iter().map(|p| root.join(p)).collect();
    md_files(&root.join("docs"), &mut paths);
    md_files(&root.join("plugin/claude"), &mut paths);
    paths.retain(|p| p.is_file());
    paths.sort();
    let read = |p: PathBuf| {
        let rel = p
            .strip_prefix(root)
            .expect("under the root")
            .display()
            .to_string();
        (rel, fs::read_to_string(&p).expect("read document"))
    };
    paths.into_iter().map(read).collect()
}

/// The help text of `agora <path> --help`, with the data kept under `home`.
fn run_help(home: &Path, path: &[String]) -> String {
    let mut cmd = Command::cargo_bin("agora").expect("agora binary");
    cmd.env("AGORA_HOME", home)
        .env("HOME", home)
        .env("AGORA_NO_SPAWN", "1");
    let out = cmd.args(path).arg("--help").timeout(CMD_TIMEOUT).output();
    let out = out.expect("run agora --help");
    assert!(out.status.success(), "agora {path:?} --help failed");
    String::from_utf8(out.stdout).expect("UTF-8 help")
}

#[test]
fn documented_agora_commands_exist() {
    let home = tempfile::tempdir().expect("home dir");
    let tree = build_tree(&mut |path: &[String]| run_help(home.path(), path), &HIDDEN);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

    let (mut invocations, mut failures) = (0, Vec::new());
    for (file, text) in documents(&root) {
        let (found, errors) = check_document(&file, &text, &tree);
        invocations += found;
        failures.extend(errors);
    }
    assert!(invocations > 0, "the scan found no invocation");
    let list = failures.join("\n");
    let list = format!("documented agora commands or flags the CLI lacks:\n{list}");
    assert!(failures.is_empty(), "{list}");
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "Commands:\n  whoami  Show identity\n  work    The work queue\n          \
        a wrapped line, not a command\n  help    Print help\n\nOptions:\n      \
        --home <HOME>  Data directory\n      --json         Print JSON\n  -h, --help         Print help\n";

    fn canned_tree() -> Tree {
        let mut help = |path: &[String]| {
            let text = match path.join(" ").as_str() {
                "" => ROOT,
                "work" => "Commands:\n  list  List\n  show  Show\n\nOptions:\n  -h, --help  Help\n",
                "work list" => "Options:\n      --limit <N>  Most recent\n  -h, --help  Help\n",
                "_supervise" => "Options:\n      --job-dir <DIR>  Job\n  -h, --help  Help\n",
                "whoami" | "work show" => "Options:\n  -h, --help  Help\n",
                other => panic!("no canned help for {other:?}"),
            };
            text.to_owned()
        };
        build_tree(&mut help, &["_supervise"])
    }

    /// The lexing and parsing helpers: quoted words, fences, prompts and assignments, and help.
    #[test]
    fn parser_helpers_split_words_fences_prompts_and_help_text() {
        let got: Vec<(String, bool)> = tokenize("chat \"see file\" 'a b' c")
            .into_iter()
            .map(|t| (t.text, t.quoted))
            .collect();
        let want = [
            ("chat", false),
            ("see file", true),
            ("a b", true),
            ("c", false),
        ];
        assert_eq!(got, want.map(|(t, q)| (t.to_owned(), q)));

        assert_eq!(fence_open("```sh"), Some("sh".to_owned()));
        assert_eq!(fence_open("``` inline ``` span"), None);
        assert!(is_fence_close("````") && !is_fence_close("```sh"));

        assert_eq!(strip_noise("$ FOO=1 BAR=x agora whoami"), "agora whoami");
        assert_eq!(strip_noise("agora chat x FOO=1"), "agora chat x FOO=1");

        let root = parse_help(ROOT);
        let children: Vec<&str> = root.children.iter().map(String::as_str).collect();
        assert_eq!(children, ["whoami", "work"]);
        let flags: Vec<(&str, bool)> = root.flags.iter().map(|(n, v)| (n.as_str(), *v)).collect();
        assert_eq!(flags, [("help", false), ("home", true), ("json", false)]);
    }

    /// Each document with the failures it must produce, one behaviour per case.
    #[rustfmt::skip]
    #[test]
    fn documents_produce_exactly_the_failures_their_invocations_earn() {
        let cases: [(&str, &[&str]); 5] = [
            // A root word that names no subcommand fails; `help` does not.
            ("```sh\nagora frobnicate\nagora help\n```\n",
             &["doc.md:2: agora frobnicate — unknown subcommand 'frobnicate'"]),
            // A flag belongs to the deepest command or is global; `--home X` does not end the path.
            ("```sh\nagora work list --limit 3 --json\nagora work show --limit 3\n\
              agora --home /tmp/x work list --since 1\nagora _supervise --job-dir D\n\
              agora _supervise --bogus\n```\n",
             &["doc.md:3: agora work show --limit 3 — unknown flag '--limit' for 'agora work show'",
               "doc.md:4: agora --home /tmp/x work list --since 1 — unknown flag '--since' for 'agora work list'",
               "doc.md:6: agora _supervise --bogus — unknown flag '--bogus' for 'agora _supervise'"]),
            // Every command of a line, and every inline span, is checked.
            ("```sh\ncd x && agora frob1 | jq .\n```\n`agora frob2`\n```console\n$(agora frob3)\n```\n",
             &["doc.md:2: agora frob1 — unknown subcommand 'frob1'",
               "doc.md:4: agora frob2 — unknown subcommand 'frob2'",
               "doc.md:6: agora frob3 — unknown subcommand 'frob3'"]),
            // Prose, an unscanned block, and a table column after two spaces name no failure.
            ("Run `agora Daemon --start` now.\n```json\nagora frobnicate\n```\n\
              ```\nagora whoami --json    daemon --mcp-http\n```\n", &[]),
            // A skip marker hides its own line and the line after it.
            ("<!-- docs-check: skip -->\n`agora frob1`\n`agora frob2` <!-- docs-check: skip -->\n\n\
              `agora frob3`\n",
             &["doc.md:5: agora frob3 — unknown subcommand 'frob3'"]),
        ];
        let tree = canned_tree();
        for (doc, want) in cases {
            assert_eq!(check_document("doc.md", doc, &tree).1, want, "document {doc:?}");
        }
    }
}
