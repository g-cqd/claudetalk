//! `place(cwd)` of work.py: the kind of directory a job runs in, from the lexical path, the repository marker and the
//! scratch roots. Filesystem reads go through the injected `PlaceProbe`.

use super::PlaceProbe;
use super::text::{basename, digit_runs, is_py_space};

/// Read limit of a `.git` file, in characters (work.py `main_checkout`).
const GITDIR_READ: usize = 4096;

/// work.py `place(cwd)` with an injected filesystem probe and scratch roots (pass ["/tmp","/private/tmp","/var/folders",
/// "/private/var/folders", <realpath TMPDIR>]).
pub fn place(cwd: Option<&str>, fs: &dyn PlaceProbe, scratch: &[String]) -> String {
    let Some(cwd) = cwd.filter(|c| !c.is_empty()) else {
        return "none".to_string();
    };
    let path = normpath(cwd);
    let mut current = path.clone();
    loop {
        let marker = join(&current, ".git");
        let is_file = fs.is_file(&marker);
        if is_file || fs.is_dir(&marker) {
            let name = is_file
                .then(|| main_checkout(fs, &marker))
                .flatten()
                .unwrap_or_else(|| basename(&current).to_string());
            let relative = relative_inside(&path, &current);
            let text = if relative == "." {
                name
            } else {
                format!("{name}/{relative}")
            };
            return digit_runs(&text, 1);
        }
        let parent = dirname(&current);
        if parent == current {
            break;
        }
        current = parent;
    }
    let scratch_hit = scratch.iter().any(|root| {
        path == *root
            || path
                .strip_prefix(root.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    });
    if scratch_hit {
        return "tmp".to_string();
    }
    let base = basename(&path);
    digit_runs(if base.is_empty() { "/" } else { base }, 1)
}

/// The name of the main checkout a linked worktree's `.git` file points at (`gitdir: MAIN/.git/worktrees/NAME`).
fn main_checkout(fs: &dyn PlaceProbe, marker: &str) -> Option<String> {
    let text = fs.read_head(marker, GITDIR_READ)?;
    let text: String = text.chars().take(GITDIR_READ).collect();
    let target = first_line(&text)
        .split_once("gitdir:")
        .map_or("", |(_, rest)| rest)
        .trim_matches(is_py_space);
    let (main, found) = match target.split_once("/.git/worktrees/") {
        Some((main, _)) => (main, true),
        None => ("", false),
    };
    if !found || main.is_empty() {
        return None;
    }
    let name = basename(main);
    (!name.is_empty()).then(|| name.to_string())
}

/// The text before the first line break, as Python's `splitlines()[0]` gives it.
fn first_line(text: &str) -> &str {
    let end = text
        .find(|c: char| {
            matches!(
                c,
                '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}'
                    ..='\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
            )
        })
        .unwrap_or(text.len());
    text.get(..end).unwrap_or("")
}

/// POSIX `posixpath.normpath`: lexical only, with no symlink resolution.
fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let mut stack: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".." || (initial == 0 && stack.is_empty()) || stack.last() == Some(&"..") {
            stack.push(comp);
        } else if !stack.is_empty() {
            stack.pop();
        }
    }
    let out = format!("{}{}", "/".repeat(initial), stack.join("/"));
    if out.is_empty() { ".".to_string() } else { out }
}

/// POSIX `posixpath.dirname`.
fn dirname(path: &str) -> String {
    let cut = path.rfind('/').map_or(0, |i| i + 1);
    let head = path.get(..cut).unwrap_or("");
    if head.chars().any(|c| c != '/') {
        head.trim_end_matches('/').to_string()
    } else {
        head.to_string()
    }
}

/// POSIX `posixpath.join` of a directory and one name.
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn components(path: &str) -> Vec<&str> {
    path.split('/').filter(|c| !c.is_empty()).collect()
}

/// `os.path.relpath(path, current)` where `current` is an ancestor of `path`: the remaining components, or `.`.
fn relative_inside(path: &str, current: &str) -> String {
    let base = components(current);
    let all = components(path);
    let rest: &[&str] = if all.starts_with(&base) {
        all.get(base.len()..).unwrap_or(&[])
    } else {
        &all
    };
    if rest.is_empty() {
        ".".to_string()
    } else {
        rest.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed filesystem: directories by path, and files by path with their content.
    #[derive(Default)]
    struct Fake {
        dirs: Vec<String>,
        files: Vec<(String, String)>,
    }

    impl PlaceProbe for Fake {
        fn is_dir(&self, p: &str) -> bool {
            self.dirs.iter().any(|d| d == p)
        }
        fn is_file(&self, p: &str) -> bool {
            self.files.iter().any(|(f, _)| f == p)
        }
        fn read_head(&self, p: &str, n: usize) -> Option<String> {
            self.files
                .iter()
                .find(|(f, _)| f == p)
                .map(|(_, content)| content.chars().take(n).collect())
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
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn place_names_a_checkout_and_its_subdirectory_with_digits_masked() {
        let fs = Fake {
            dirs: vec!["/Users/me/proj-12/.git".into()],
            ..Fake::default()
        };
        assert_eq!(
            place(Some("/Users/me/proj-12/sub3"), &fs, &scratch()),
            "proj-#/sub#"
        );
        assert_eq!(place(Some("/Users/me/proj-12"), &fs, &scratch()), "proj-#");
        assert_eq!(
            place(Some("/Users/me/proj-12/sub3/"), &fs, &scratch()),
            "proj-#/sub#"
        );
    }

    #[test]
    fn place_without_a_repository_is_the_directory_name_or_tmp() {
        let fs = Fake::default();
        assert_eq!(
            place(Some("/Users/me/proj-12/sub3"), &fs, &scratch()),
            "sub#"
        );
        assert_eq!(place(Some("/a/b/c-99999"), &fs, &scratch()), "c-#");
        assert_eq!(place(Some("/"), &fs, &scratch()), "/");
        assert_eq!(place(Some("../a/b"), &fs, &scratch()), "b");
        assert_eq!(place(Some("/tmpx/y"), &fs, &scratch()), "y");
        assert_eq!(place(Some("/tmp/x"), &fs, &scratch()), "tmp");
        assert_eq!(place(Some("/tmp"), &fs, &scratch()), "tmp");
        assert_eq!(place(Some("/var/folders/ab/T/x"), &fs, &scratch()), "tmp");
    }

    #[test]
    fn place_of_a_linked_worktree_is_the_main_checkouts_name() {
        let fs = Fake {
            files: vec![(
                "/Users/me/wt/.git".into(),
                "gitdir: /Users/me/app/.git/worktrees/wt\n".into(),
            )],
            ..Fake::default()
        };
        assert_eq!(
            place(Some("/Users/me/wt/Packages/Core"), &fs, &scratch()),
            "app/Packages/Core"
        );
        assert_eq!(place(Some("/Users/me/wt"), &fs, &scratch()), "app");
        let no_newline = Fake {
            files: vec![(
                "/Users/me/wt2/.git".into(),
                "gitdir: /Users/me/app/.git/worktrees/wt2".into(),
            )],
            ..Fake::default()
        };
        assert_eq!(place(Some("/Users/me/wt2"), &no_newline, &scratch()), "app");
    }

    #[test]
    fn place_of_a_submodule_falls_back_to_its_own_directory_name() {
        let fs = Fake {
            files: vec![(
                "/Users/me/sub/.git".into(),
                "gitdir: /elsewhere/nothing\n".into(),
            )],
            ..Fake::default()
        };
        assert_eq!(place(Some("/Users/me/sub/x"), &fs, &scratch()), "sub/x");
    }

    #[test]
    fn place_of_no_or_empty_cwd_is_none() {
        let fs = Fake::default();
        assert_eq!(place(None, &fs, &scratch()), "none");
        assert_eq!(place(Some(""), &fs, &scratch()), "none");
    }
}
