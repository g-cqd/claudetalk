//! Splitting a token stream into simple commands and finding a command's program, past assignments and wrappers.

use super::text::{basename, has_fd_dup_suffix, is_assignment, is_redirect, listed};

const WRAPPERS: &[&str] = &[
    "env",
    "xcrun",
    "time",
    "nice",
    "caffeinate",
    "command",
    "exec",
    "timeout",
    "gtimeout",
];

const OPERATORS: &[&str] = &[
    ";", "&&", "||", "|", "&", "\n", "(", ")", "{", "}", "|&", ";;",
];
const OPERATOR_CHARS: &str = ";&|()";
const SCRIPT_EXTENSIONS: &[&str] = &[".sh", ".py", ".rb", ".js", ".pl"];

/// Whether an operator token ends a simple command: an operator word, or one made of `;&|()` characters only.
fn is_operator(token: &str) -> bool {
    listed(OPERATORS, token)
        || (!token.is_empty() && token.chars().all(|c| OPERATOR_CHARS.contains(c)))
}

/// The simple commands of a token stream, split at shell operators, without redirections and their targets.
pub(super) fn simple_commands(tokens: &[String]) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut current = Vec::new();
    let mut skip = false;
    for token in tokens {
        if skip {
            skip = false;
        } else if is_operator(token) {
            if !current.is_empty() {
                commands.push(std::mem::take(&mut current));
            }
        } else if is_redirect(token) {
            skip = !has_fd_dup_suffix(token);
        } else {
            current.push(token.clone());
        }
    }
    if !current.is_empty() {
        commands.push(current);
    }
    commands
}

/// A simple command without its leading `VAR=value` assignments and wrappers (env, xcrun, time, timeout...), with a
/// wrapper's options, and timeout's duration.
pub(super) fn program_part(command: &[String]) -> &[String] {
    let mut index = 0;
    while let Some(token) = command.get(index) {
        let name = basename(token);
        if is_assignment(token) {
            index += 1;
        } else if listed(WRAPPERS, name) {
            index += 1;
            while command.get(index).is_some_and(|t| t.starts_with('-')) && name != "xcrun" {
                index += 1;
            }
            if matches!(name, "timeout" | "gtimeout") && index < command.len() {
                index += 1;
            }
        } else {
            break;
        }
    }
    command.get(index..).unwrap_or(&[])
}

/// A script's interpreter argument: a path, or a name with a script extension (`.sh`, `.py`, `.rb`, `.js`, `.pl`).
/// `$` in work.py also matches before one trailing newline, so that newline is allowed here.
pub(super) fn has_script_extension(script: &str) -> bool {
    let stem = script.strip_suffix('\n').unwrap_or(script);
    SCRIPT_EXTENSIONS.iter().any(|ext| stem.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn simple_commands_drop_redirect_targets_and_keep_a_duplicated_fd_stray() {
        assert_eq!(
            simple_commands(&argv(&["make", "test", ">", "log.txt", "x"])),
            [argv(&["make", "test", "x"])]
        );
        assert_eq!(
            simple_commands(&argv(&["make", "2", ">&", "1", "x"])),
            [argv(&["make", "2", "x"])]
        );
        assert_eq!(
            simple_commands(&argv(&["a", "{", "b", "}", "c", "", "d", "&&", "e"])),
            [
                argv(&["a"]),
                argv(&["b"]),
                argv(&["c", "", "d"]),
                argv(&["e"])
            ]
        );
    }

    #[test]
    fn program_part_skips_assignments_wrappers_and_timeouts_duration() {
        assert_eq!(
            program_part(&argv(&["env", "-i", "FOO=2", "swift", "test"])),
            argv(&["swift", "test"])
        );
        assert_eq!(
            program_part(&argv(&["timeout", "-s", "KILL", "10", "swift", "test"])),
            argv(&["10", "swift", "test"])
        );
        assert_eq!(
            program_part(&argv(&["nice", "-n", "10", "make"])),
            argv(&["10", "make"])
        );
        assert_eq!(
            program_part(&argv(&["xcrun", "-sdk", "macosx", "swiftc"])),
            argv(&["-sdk", "macosx", "swiftc"])
        );
        assert_eq!(
            program_part(&argv(&["gtimeout", "5"])),
            Vec::<String>::new()
        );
    }
}
