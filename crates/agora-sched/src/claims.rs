//! Names, kinds, text checks, file claims and the command digest (ref-queue-cli sections 0, 2, 3).

use crate::graph::overlap;
use crate::pyfmt::{is_printable, repr_str};
use sha2::{Digest, Sha256};

/// work.py MAX_ITEMS: arguments of one command, and files of one task.
const MAX_ITEMS: usize = 1000;
/// work.py MAX_COMMAND_BYTES: UTF-8 bytes of a command, plus one per argument.
const MAX_COMMAND_BYTES: usize = 256 * 1024;
const NAME_MAX: usize = 128;
const KIND_MAX: usize = 64;

fn is_alnum_or_underscore(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_name_rest(c: char) -> bool {
    is_alnum_or_underscore(c) || matches!(c, '.' | ':' | '@' | '-')
}

fn is_kind_rest(c: char) -> bool {
    is_alnum_or_underscore(c) || c == '-'
}

/// A non-empty value of at most `max_len` bytes whose first char satisfies `head` and whose other chars satisfy
/// `rest`. Every accepted char is ASCII, so the byte length is the character count.
fn well_formed(
    value: &str,
    max_len: usize,
    head: fn(char) -> bool,
    rest: fn(char) -> bool,
) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value
            .chars()
            .enumerate()
            .all(|(index, c)| if index == 0 { head(c) } else { rest(c) })
}

/// NAME = [A-Za-z0-9_][A-Za-z0-9_.:@-]{0,127}; message "{what} {repr} is not valid: use up to 128 letters, digits,
/// _ . : @ or -, starting with a letter, a digit or _".
pub fn validate_name(what: &str, value: &str) -> Result<(), String> {
    if well_formed(value, NAME_MAX, is_alnum_or_underscore, is_name_rest) {
        Ok(())
    } else {
        Err(format!(
            "{what} {} is not valid: use up to 128 letters, digits, _ . : @ or -, starting with a letter, a digit or _",
            repr_str(value)
        ))
    }
}

/// KIND = [A-Za-z0-9_][A-Za-z0-9_-]{0,63}.
pub fn validate_kind(kind: &str) -> Result<(), String> {
    if well_formed(kind, KIND_MAX, is_alnum_or_underscore, is_kind_rest) {
        Ok(())
    } else {
        Err(format!(
            "kind {} is not valid: use up to 64 letters, digits, _ or -",
            repr_str(kind)
        ))
    }
}

/// work.py check_text (control/invisible chars refused, length limit). The length is checked first, in characters.
pub fn check_text(what: &str, value: &str, limit: usize) -> Result<(), String> {
    if value.chars().count() > limit {
        return Err(format!("{what} is longer than {limit} characters"));
    }
    match value.chars().find(|c| !is_printable(*c)) {
        Some(hidden) => Err(format!(
            "{what} holds the control or invisible character {}",
            repr_str(&hidden.to_string())
        )),
        None => Ok(()),
    }
}

/// work.py check_command (non-empty, <= 1000 args, <= 262144 bytes, no NUL).
pub fn check_command(argv: &[String]) -> Result<(), String> {
    if argv.is_empty() {
        return Err("no command after --".to_string());
    }
    let bytes = argv.iter().fold(0usize, |total, arg| {
        total.saturating_add(arg.len().saturating_add(1))
    });
    if argv.len() > MAX_ITEMS || bytes > MAX_COMMAND_BYTES {
        return Err(format!(
            "the command is longer than {MAX_ITEMS} arguments or {MAX_COMMAND_BYTES} bytes"
        ));
    }
    if argv.iter().any(|arg| arg.contains('\0')) {
        return Err("the command holds a NUL character".to_string());
    }
    Ok(())
}

/// Lexical canonical path: "~" / "~/" expanded with `home`, relative joined to `cwd`, "." and ".." and "//" removed,
/// trailing "/" removed (root stays "/"). Empty or NUL → Err("'' is not a file path"). (Symlinks are resolved by the
/// CLIENT with std::fs::canonicalize of the longest existing prefix before signing; the server only re-normalises.)
pub fn canonical_path(path: &str, cwd: &str, home: Option<&str>) -> Result<String, String> {
    if path.is_empty() || path.contains('\0') {
        return Err(format!("{} is not a file path", repr_str(path)));
    }
    let expanded = match home {
        Some(home) if path == "~" => home.to_string(),
        Some(home) => match path.strip_prefix("~/") {
            Some(rest) => format!("{home}/{rest}"),
            None => path.to_string(),
        },
        None => path.to_string(),
    };
    let joined = if expanded.starts_with('/') {
        expanded
    } else {
        format!("{cwd}/{expanded}")
    };
    Ok(normalise(&joined))
}

/// Drops empty and "." parts, pops on ".." (never above the root), and anchors the result at "/".
fn normalise(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

/// First holder (task id, its files) whose files overlap any of `files` (graph::overlap); returns (holder id, held path).
pub fn first_conflict<'a>(
    files: &[String],
    holders: &'a [(String, Vec<String>)],
) -> Option<(&'a str, &'a str)> {
    holders.iter().find_map(|(holder, held)| {
        held.iter()
            .find(|held_path| files.iter().any(|file| overlap(file, held_path)))
            .map(|held_path| (holder.as_str(), held_path.as_str()))
    })
}

/// D5 digest: hex sha256 of serde_json::to_vec(&json!(["agora-work/v1", argv, cwd, files])).
pub fn digest(argv: &[String], cwd: &str, files: &[String]) -> String {
    let body = serde_json::json!(["agora-work/v1", argv, cwd, files]);
    hex_sha256(body.to_string().as_bytes())
}

/// work.py digest: sha256 of json.dumps([argv, cwd], separators=(",",":")) with ensure_ascii. Vector:
/// (["touch","/tmp/x"], "/tmp/proj") -> 2a76c0cd4983dc2de985784b48d45899087d92722178e600ef7649d8ba05201f.
pub fn workpy_digest(argv: &[String], cwd: &str) -> String {
    let mut out = String::from("[[");
    for (index, arg) in argv.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_ascii_json_string(&mut out, arg);
    }
    out.push_str("],");
    push_ascii_json_string(&mut out, cwd);
    out.push(']');
    hex_sha256(out.as_bytes())
}

/// Appends `value` as a JSON string the way Python's json.dumps does with ensure_ascii: short escapes for the
/// quote, backslash and the control characters it names, \uXXXX (lowercase) for everything outside 0x20..=0x7e, and
/// UTF-16 surrogate pairs for astral characters.
fn push_ascii_json_string(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units).iter() {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn names_accept_the_documented_shapes() {
        for name in ["a", "_x", "9lives", "job-1.2:3@4"] {
            assert_eq!(validate_name("task id", name), Ok(()), "{name}");
        }
        let longest = "n".repeat(128);
        assert_eq!(validate_name("task id", &longest), Ok(()));
    }

    #[test]
    fn names_reject_escapes_length_and_leading_dash() {
        for name in ["../escape", "a/b", "-lead", "", "a b", "é"] {
            assert!(validate_name("task id", name).is_err(), "{name}");
        }
        let too_long = "n".repeat(129);
        assert!(validate_name("task id", &too_long).is_err());
    }

    #[test]
    fn name_error_uses_the_work_py_message_with_repr() {
        assert_eq!(
            validate_name("task id", "../x"),
            Err("task id '../x' is not valid: use up to 128 letters, digits, _ . : @ or -, starting with a letter, a digit or _".to_string())
        );
    }

    #[test]
    fn kinds_accept_word_and_dash_forms_and_reject_spaces() {
        for kind in ["other", "data-loss", "a_b", "1"] {
            assert_eq!(validate_kind(kind), Ok(()), "{kind}");
        }
        for kind in ["a b", "kind.x", "-x", ""] {
            assert!(validate_kind(kind).is_err(), "{kind}");
        }
        assert_eq!(validate_kind(&"k".repeat(64)), Ok(()));
        assert!(validate_kind(&"k".repeat(65)).is_err());
        assert_eq!(
            validate_kind("a b"),
            Err("kind 'a b' is not valid: use up to 64 letters, digits, _ or -".to_string())
        );
    }

    #[test]
    fn check_text_refuses_controls_zero_width_and_bidi_overrides() {
        assert_eq!(check_text("title", "hello, wörld", 4000), Ok(()));
        assert_eq!(
            check_text("title", "a\u{1b}b", 4000),
            Err("title holds the control or invisible character '\\x1b'".to_string())
        );
        assert_eq!(
            check_text("title", "a\u{200b}b", 4000),
            Err("title holds the control or invisible character '\\u200b'".to_string())
        );
        assert_eq!(
            check_text("title", "a\u{202e}b", 4000),
            Err("title holds the control or invisible character '\\u202e'".to_string())
        );
        assert_eq!(
            check_text("note", "a\tb", 4000),
            Err("note holds the control or invisible character '\\t'".to_string())
        );
    }

    #[test]
    fn check_text_limit_counts_characters_and_comes_first() {
        assert_eq!(check_text("title", "éééé", 4), Ok(()));
        assert_eq!(
            check_text("title", "éééée", 4),
            Err("title is longer than 4 characters".to_string())
        );
        // Length is judged before the printable check, as in work.py.
        assert_eq!(
            check_text("title", "\u{1}\u{1}\u{1}\u{1}\u{1}", 4),
            Err("title is longer than 4 characters".to_string())
        );
    }

    #[test]
    fn check_command_refuses_empty_and_nul() {
        assert_eq!(check_command(&[]), Err("no command after --".to_string()));
        assert_eq!(check_command(&args(&["echo", "hi"])), Ok(()));
        assert_eq!(
            check_command(&args(&["a\0b"])),
            Err("the command holds a NUL character".to_string())
        );
    }

    #[test]
    fn check_command_limits_items_and_bytes_exactly() {
        let limit_msg = "the command is longer than 1000 arguments or 262144 bytes";
        let many = vec![String::new(); 1000];
        assert_eq!(check_command(&many), Ok(()));
        let too_many = vec![String::new(); 1001];
        assert_eq!(check_command(&too_many), Err(limit_msg.to_string()));
        // One byte per argument is charged on top of its length: 262143 + 1 = 262144 is allowed, one more is not.
        assert_eq!(check_command(&[String::from("x").repeat(262143)]), Ok(()));
        assert_eq!(
            check_command(&[String::from("x").repeat(262144)]),
            Err(limit_msg.to_string())
        );
    }

    #[test]
    fn canonical_path_expands_home_and_joins_relative_paths() {
        assert_eq!(
            canonical_path("~", "/w", Some("/h/u")),
            Ok("/h/u".to_string())
        );
        assert_eq!(
            canonical_path("~/x/../y/", "/w", Some("/h/u")),
            Ok("/h/u/y".to_string())
        );
        assert_eq!(canonical_path("~", "/w", None), Ok("/w/~".to_string()));
        assert_eq!(
            canonical_path("a/./b", "/w", None),
            Ok("/w/a/b".to_string())
        );
        assert_eq!(
            canonical_path("/abs/x", "/w", None),
            Ok("/abs/x".to_string())
        );
    }

    #[test]
    fn canonical_path_removes_dots_double_slashes_and_trailing_slash() {
        assert_eq!(canonical_path("/../../x", "/", None), Ok("/x".to_string()));
        assert_eq!(
            canonical_path("//a///b//", "/", None),
            Ok("/a/b".to_string())
        );
        assert_eq!(canonical_path("/a/b/.", "/", None), Ok("/a/b".to_string()));
        assert_eq!(canonical_path("/", "/w", None), Ok("/".to_string()));
        assert_eq!(canonical_path("..", "/", None), Ok("/".to_string()));
        assert_eq!(canonical_path("x", "/w/..", None), Ok("/x".to_string()));
    }

    #[test]
    fn canonical_path_refuses_empty_and_nul_with_repr() {
        assert_eq!(
            canonical_path("", "/w", None),
            Err("'' is not a file path".to_string())
        );
        assert_eq!(
            canonical_path("a\0b", "/w", None),
            Err("'a\\x00b' is not a file path".to_string())
        );
    }

    #[test]
    fn first_conflict_follows_path_prefix_rules() {
        let holders = vec![("t1".to_string(), args(&["/a/bc"]))];
        assert_eq!(first_conflict(&args(&["/a/b"]), &holders), None);
        let holders = vec![("t2".to_string(), args(&["/a/b/c"]))];
        assert_eq!(
            first_conflict(&args(&["/a/b"]), &holders),
            Some(("t2", "/a/b/c"))
        );
        assert_eq!(first_conflict(&args(&["/a/b"]), &[]), None);
    }

    #[test]
    fn first_conflict_reports_equal_root_and_first_holder_in_order() {
        let holders = vec![("t1".to_string(), args(&["/x"]))];
        assert_eq!(first_conflict(&args(&["/x"]), &holders), Some(("t1", "/x")));
        // The root overlaps everything.
        let holders = vec![("t3".to_string(), args(&["/"]))];
        assert_eq!(
            first_conflict(&args(&["/deep/file"]), &holders),
            Some(("t3", "/"))
        );
        let holders = vec![
            ("first".to_string(), args(&["/p"])),
            ("second".to_string(), args(&["/p/q"])),
        ];
        assert_eq!(
            first_conflict(&args(&["/p/q/r"]), &holders),
            Some(("first", "/p"))
        );
    }

    #[test]
    fn digest_is_stable_lowercase_hex_and_sensitive_to_each_input() {
        let argv = args(&["make", "test"]);
        let files = args(&["/w/a"]);
        let base = digest(&argv, "/w", &files);
        assert_eq!(base.len(), 64);
        assert!(
            base.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_eq!(base, digest(&argv, "/w", &files));
        assert_ne!(base, digest(&args(&["make", "check"]), "/w", &files));
        assert_ne!(base, digest(&argv, "/other", &files));
        assert_ne!(base, digest(&argv, "/w", &args(&["/w/b"])));
        assert_ne!(base, digest(&argv, "/w", &[]));
    }

    #[test]
    fn digest_pins_the_d5_document_shape() {
        // sha256 of ["agora-work/v1",["touch","/tmp/x"],"/tmp/proj",["/tmp/x","/tmp/y"]] with compact separators.
        assert_eq!(
            digest(
                &args(&["touch", "/tmp/x"]),
                "/tmp/proj",
                &args(&["/tmp/x", "/tmp/y"])
            ),
            "45a52082bd7bdb917d33061ba9f37de78d4d44598c33cf20ebe90f0af43f1244"
        );
    }

    #[test]
    fn workpy_digest_matches_python_vectors() {
        assert_eq!(
            workpy_digest(&args(&["touch", "/tmp/x"]), "/tmp/proj"),
            "2a76c0cd4983dc2de985784b48d45899087d92722178e600ef7649d8ba05201f"
        );
        assert_eq!(
            workpy_digest(
                &args(&["echo", "h\u{e9}llo \"q\" \\ \n \u{1F600}"]),
                "/tmp/caf\u{e9}"
            ),
            "045340f94890bbda6a07f290671267b31fde546aba9a95a895a53b17f1d766f1"
        );
        assert_eq!(
            workpy_digest(
                &args(&["printf", "%s\t\u{7f}\u{1}\u{8}\u{c}\r", "\u{2028}"]),
                "/tmp/a b/\""
            ),
            "1bd5b78cf9661d38ee332b235d4759e4a568801dd993caeeb6e3dfab6a47acd6"
        );
    }
}
