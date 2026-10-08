//! Token matchers and digests for command signatures. The regexes of work.py (`UUID`, `HEX_ID`, `LONG_NUMBER`,
//! `DIGITS`, `ASSIGNMENT`, `REDIRECT`, `[\w.-]+/[\w.-]+`) are written by hand, because agora-sched has no regex crate.
//! `\d` and `\w` are ASCII here.

use sha2::{Digest, Sha256};

/// Python's `str.isspace`: Unicode whitespace plus the separators U+001C to U+001F.
pub(super) fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python's `" ".join(text.split())`.
pub(super) fn collapse(text: &str) -> String {
    text.split(is_py_space)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Lowercase hex of the sha256 of `bytes`.
pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The first `len` hex digits of the sha256 of `text`.
pub(super) fn digest(text: &str, len: usize) -> String {
    sha256_hex(text.as_bytes()).chars().take(len).collect()
}

/// work.py `compact`: `text` when it has at most 256 code points, otherwise its first 222 code points, `~`, and
/// the 32-digit digest of all of it.
pub(super) fn compact(text: &str) -> String {
    const LIMIT: usize = 256;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let head: String = text.chars().take(LIMIT - 34).collect();
    format!("{head}~{}", digest(text, 32))
}

/// Replaces each maximal run of characters that `in_run` accepts with `map(run)`.
fn map_runs(text: &str, in_run: fn(char) -> bool, map: impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    for c in text.chars() {
        if in_run(c) {
            run.push(c);
        } else {
            if !run.is_empty() {
                out.push_str(&map(&run));
                run.clear();
            }
            out.push(c);
        }
    }
    if !run.is_empty() {
        out.push_str(&map(&run));
    }
    out
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `DIGITS` (`min` 1) and `LONG_NUMBER` (`min` 3): each run of at least `min` ASCII digits becomes `#`.
pub(super) fn digit_runs(text: &str, min: usize) -> String {
    map_runs(
        text,
        |c| c.is_ascii_digit(),
        |run| {
            if run.len() >= min {
                "#".to_string()
            } else {
                run.to_string()
            }
        },
    )
}

const UUID_GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
const UUID_LEN: usize = 36;

fn is_uuid_at(chars: &[char], start: usize) -> bool {
    let mut pos = start;
    for (group, len) in UUID_GROUPS.iter().enumerate() {
        if group > 0 {
            if chars.get(pos) != Some(&'-') {
                return false;
            }
            pos += 1;
        }
        for _ in 0..*len {
            if !chars.get(pos).is_some_and(char::is_ascii_hexdigit) {
                return false;
            }
            pos += 1;
        }
    }
    true
}

/// `UUID` (unanchored) replaced by `<id>`.
fn uuid_sub(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if is_uuid_at(&chars, i) {
            out.push_str("<id>");
            i += UUID_LEN;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// `HEX_ID` without its lookahead: a whole word of seven or more lowercase hex digits that holds a digit.
fn is_hex_id(run: &str) -> bool {
    run.len() >= 7
        && run
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && run.bytes().any(|b| b.is_ascii_digit())
}

fn hex_id_sub(text: &str) -> String {
    map_runs(text, is_word, |run| {
        if is_hex_id(run) {
            "<id>".to_string()
        } else {
            run.to_string()
        }
    })
}

/// work.py `scrub`: ids become `<id>`, then runs of three or more digits become `#`. The order matters.
pub(super) fn scrub(text: &str) -> String {
    digit_runs(&hex_id_sub(&uuid_sub(text)), 3)
}

/// The inside of work.py `path_tag`: the last path component, with its ids and digits masked.
pub(super) fn path_tag_inner(token: &str) -> String {
    let last = token.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    digit_runs(&scrub(if last.is_empty() { "/" } else { last }), 1)
}

/// Whether a token names a file: it starts with `/`, `~`, `./` or `../`, or holds a `/` and is neither a URL nor an
/// owner/name pair such as `origin/main`.
pub(super) fn looks_like_path(token: &str) -> bool {
    if ["/", "~", "./", "../"]
        .iter()
        .any(|p| token.starts_with(*p))
    {
        return true;
    }
    token.contains('/')
        && !token.starts_with("http://")
        && !token.starts_with("https://")
        && !is_owner_name(token)
}

/// `[\w.-]+/[\w.-]+` as a whole token.
fn is_owner_name(token: &str) -> bool {
    match token.split_once('/') {
        Some((owner, name)) => is_name_part(owner) && is_name_part(name),
        None => false,
    }
}

fn is_name_part(part: &str) -> bool {
    !part.is_empty() && part.chars().all(|c| is_word(c) || c == '.' || c == '-')
}

/// `ASSIGNMENT` used with `match`: `[A-Za-z_][A-Za-z0-9_]*=` at the start of the token.
pub(super) fn is_assignment(token: &str) -> bool {
    let mut chars = token.chars();
    if !matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') {
        return false;
    }
    for c in chars {
        if c == '=' {
            return true;
        }
        if !is_word(c) {
            return false;
        }
    }
    false
}

/// work.py `normal_token`: an assignment or `--flag=value` keeps its name and normalizes its value, which is
/// repeated while the value is again such a token. Otherwise a path becomes its tag and anything else is scrubbed.
pub(super) fn normal_token(token: &str) -> String {
    let mut out = String::new();
    let mut rest = token;
    loop {
        if is_assignment(rest) || (rest.starts_with("--") && rest.contains('=')) {
            let (name, value) = rest.split_once('=').unwrap_or((rest, ""));
            out.push_str(name);
            out.push('=');
            if value.is_empty() {
                return out;
            }
            rest = value;
        } else {
            if looks_like_path(rest) {
                out.push_str(&format!("<p:{}>", path_tag_inner(rest)));
            } else {
                out.push_str(&scrub(rest));
            }
            return out;
        }
    }
}

/// work.py `flag_key`: a flag with a path glued to it (`-I/x/y`) keeps its prefix, the path's tag, and a six-digit
/// digest of the path, so that two paths stay apart without being written out.
pub(super) fn flag_key(flag: &str) -> String {
    let cut = if flag.contains('/') {
        flag.find('/')
    } else {
        flag.find('~')
    };
    let Some(cut) = cut else {
        return flag.to_string();
    };
    let prefix = flag.get(..cut).unwrap_or("");
    let path = flag.get(cut..).unwrap_or("");
    format!("{prefix}<p:{}~{}>", path_tag_inner(path), digest(path, 6))
}

/// `[a-z][a-z0-9-]*` as a whole token.
pub(super) fn is_subcommand(word: &str) -> bool {
    let mut chars = word.chars();
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `REDIRECT` as a whole token: `\d*[<>]+&?\d*` or `&>+`.
pub(super) fn is_redirect(token: &str) -> bool {
    if let Some(rest) = token.strip_prefix('&') {
        return !rest.is_empty() && rest.chars().all(|c| c == '>');
    }
    let after_digits = token.trim_start_matches(|c: char| c.is_ascii_digit());
    let after_marks = after_digits.trim_start_matches(['<', '>']);
    if after_marks.len() == after_digits.len() {
        return false;
    }
    let after_amp = after_marks.strip_prefix('&').unwrap_or(after_marks);
    after_amp.chars().all(|c| c.is_ascii_digit())
}

/// `&\d$`: a file-descriptor duplication such as `2>&1`, which takes no target word.
pub(super) fn has_fd_dup_suffix(token: &str) -> bool {
    matches!(token.as_bytes(), [.., b'&', d] if d.is_ascii_digit())
}

pub(super) fn listed(list: &[&str], word: &str) -> bool {
    list.contains(&word)
}

/// POSIX `posixpath.basename`.
pub(super) fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_masks_ids_then_long_numbers() {
        assert_eq!(scrub("abc1234"), "<id>");
        assert_eq!(scrub("1234567"), "<id>");
        assert_eq!(scrub("deadbeef"), "deadbeef");
        assert_eq!(scrub("abc123"), "abc#");
        assert_eq!(scrub("xabc1234"), "xabc#");
        assert_eq!(scrub("abc_1234567"), "abc_#");
        assert_eq!(scrub("12345"), "#");
        assert_eq!(scrub("12"), "12");
        assert_eq!(scrub("123e4567-e89b-12d3-a456-426614174000"), "<id>");
        assert_eq!(scrub("10.0.1.200"), "10.0.1.#");
        assert_eq!(scrub("aaaaaaa1 bbbbbbb2"), "<id> <id>");
    }

    #[test]
    fn looks_like_path_tells_paths_from_owner_name_pairs() {
        assert!(!looks_like_path("origin/main"));
        assert!(!looks_like_path("Packages/Core"));
        assert!(looks_like_path("a/b/c"));
        assert!(!looks_like_path("https://x/y"));
        assert!(looks_like_path("./x"));
        assert!(looks_like_path("../x"));
        assert!(looks_like_path("~/x"));
        assert!(looks_like_path("x/"));
        assert!(!looks_like_path("plain"));
    }

    #[test]
    fn path_tag_keeps_the_last_component_with_digits_masked() {
        assert_eq!(path_tag_inner("origin/main"), "main");
        assert_eq!(path_tag_inner("/x/y/run-12.sh"), "run-#.sh");
        assert_eq!(path_tag_inner("x/"), "x");
        assert_eq!(path_tag_inner("/"), "/");
    }

    #[test]
    fn normal_token_keeps_names_and_normalizes_values() {
        assert_eq!(normal_token("FOO=/x/y/12"), "FOO=<p:#>");
        assert_eq!(normal_token("--out=/x/run-12.sh"), "--out=<p:run-#.sh>");
        assert_eq!(normal_token("--a="), "--a=");
        assert_eq!(normal_token("9x=1"), "9x=1");
        assert_eq!(normal_token("x=--a=/p"), "x=--a=<p:p>");
        assert_eq!(normal_token("a=b=c"), "a=b=c");
        assert_eq!(normal_token("--x=y=/q/12"), "--x=y=<p:#>");
    }

    #[test]
    fn flag_key_glues_a_path_tag_and_a_digest() {
        assert_eq!(flag_key("-I/x/y"), "-I<p:y~62b0ea>");
        assert_eq!(flag_key("-Iabc~x"), "-Iabc<p:~x~52fa21>");
        assert_eq!(flag_key("~"), "<p:~~7ace43>");
        assert_eq!(flag_key("--jobs"), "--jobs");
    }

    #[test]
    fn redirect_matches_work_py_fullmatch() {
        for token in ["2>&1", ">&", "&>", "&>>", "2>", "<>", "2>&12"] {
            assert!(is_redirect(token), "{token}");
        }
        for token in ["&", "", "12", "&&", "2&>", "3>&-", "1>&2&", "<in"] {
            assert!(!is_redirect(token), "{token}");
        }
        assert!(has_fd_dup_suffix("2>&1"));
        assert!(!has_fd_dup_suffix(">&"));
        assert!(!has_fd_dup_suffix("2>&12"));
    }

    #[test]
    fn assignment_needs_a_name_then_equals_at_the_start() {
        assert!(is_assignment("a=b=c"));
        assert!(is_assignment("_x1="));
        assert!(!is_assignment("9x=1"));
        assert!(!is_assignment("x-y=1"));
        assert!(!is_assignment("=x"));
    }

    #[test]
    fn compact_keeps_short_text_and_digests_long_text() {
        let short = "y".repeat(256);
        assert_eq!(compact(&short), short);
        let long = "ab".repeat(200);
        assert_eq!(
            compact(&long),
            format!("{}~5aafbd87667a89353855126467426161", "ab".repeat(111))
        );
        assert_eq!(compact(&long).chars().count(), 255);
    }
}
