//! The managed block `agora install` keeps inside shared instruction files (AGENTS.md ...).

use std::fmt;

/// First line of the managed block.
pub const BEGIN: &str =
    "<!-- agora:begin (managed by `agora install`; edits inside are replaced) -->";
/// Last line of the managed block.
pub const END: &str = "<!-- agora:end -->";

/// A file whose markers cannot be handled safely; nothing is written for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerError {
    /// BEGIN without a following END.
    BeginWithoutEnd,
    /// END before (or without) BEGIN.
    EndBeforeBegin,
    /// More than one block.
    MultipleBlocks,
}

impl fmt::Display for MarkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BeginWithoutEnd => "agora:begin marker without a matching agora:end",
            Self::EndBeforeBegin => "agora:end marker before (or without) agora:begin",
            Self::MultipleBlocks => "more than one agora block",
        })
    }
}

impl std::error::Error for MarkerError {}

/// Replace the single existing block (BEGIN .. END, inclusive) with BEGIN + `body` + END in place,
/// else append it after one blank line (nothing before it when `existing` is empty). The result ends
/// with `\n`; every byte outside the block is preserved (CRLF input stays CRLF outside the block).
pub fn upsert_block(existing: &str, body: &str) -> Result<String, MarkerError> {
    let lines = split_lines(existing);
    let nl = newline_of(existing);
    let block = render_block(body, nl);
    Ok(match locate(&lines)? {
        Some((begin, end)) => {
            format!(
                "{}{block}{}",
                lines[..begin].concat(),
                lines[end + 1..].concat()
            )
        }
        None if existing.is_empty() => block,
        None if existing.ends_with('\n') => format!("{existing}{nl}{block}"),
        None => format!("{existing}{nl}{nl}{block}"),
    })
}

/// `Ok(None)`: no block. Otherwise the text without the block and one adjacent blank line; never
/// touches other text. Errors as `upsert_block`.
pub fn remove_block(existing: &str) -> Result<Option<String>, MarkerError> {
    let lines = split_lines(existing);
    let Some((begin, end)) = locate(&lines)? else {
        return Ok(None);
    };
    let (first, last) = if begin > 0 && is_blank(lines[begin - 1]) {
        (begin - 1, end)
    } else if lines.get(end + 1).copied().is_some_and(is_blank) {
        (begin, end + 1)
    } else {
        (begin, end)
    };
    Ok(Some(format!(
        "{}{}",
        lines[..first].concat(),
        lines[last + 1..].concat()
    )))
}

/// True when `existing` contains a BEGIN line.
pub fn has_block(existing: &str) -> bool {
    existing.lines().any(|line| is_marker(line, BEGIN))
}

/// Split into lines, each keeping its terminator, so that concatenating the pieces restores the text.
fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// The newline the file already uses: `\r\n` when it has a CRLF line, else `\n`.
fn newline_of(existing: &str) -> &'static str {
    if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// Whether a line is the marker `marker`, ignoring a trailing `\r` and surrounding whitespace.
fn is_marker(line: &str, marker: &str) -> bool {
    line.trim() == marker
}

/// Whether a line holds nothing but whitespace (a blank line).
fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// Indices of every line that is `marker`.
fn positions(lines: &[&str], marker: &str) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_marker(line, marker))
        .map(|(index, _)| index)
        .collect()
}

/// Index of the BEGIN and END lines of the single block; `None` when there is no marker at all.
fn locate(lines: &[&str]) -> Result<Option<(usize, usize)>, MarkerError> {
    let begins = positions(lines, BEGIN);
    let ends = positions(lines, END);
    let (begin, end) = match (begins.as_slice(), ends.as_slice()) {
        ([], []) => return Ok(None),
        ([], _) => return Err(MarkerError::EndBeforeBegin),
        (_, []) => return Err(MarkerError::BeginWithoutEnd),
        ([first, ..], [last, ..]) if last < first => return Err(MarkerError::EndBeforeBegin),
        ([only], [close]) => (*only, *close),
        _ => return Err(MarkerError::MultipleBlocks),
    };
    Ok(Some((begin, end)))
}

/// The block as text: BEGIN, each body line, END, every line ending in `nl`. The body's own line
/// endings are normalized to `nl`, and a missing final newline is supplied.
fn render_block(body: &str, nl: &str) -> String {
    let mut block = format!("{BEGIN}{nl}");
    for line in body.lines() {
        block.push_str(line);
        block.push_str(nl);
    }
    block.push_str(END);
    block.push_str(nl);
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(existing: &str, body: &str) -> String {
        upsert_block(existing, body).expect("upsert")
    }

    fn rm(existing: &str) -> Option<String> {
        remove_block(existing).expect("remove")
    }

    /// The block exactly as `upsert_block` writes it into an LF file.
    fn block(body: &str) -> String {
        format!("{BEGIN}\n{body}{END}\n")
    }

    #[test]
    fn empty_file_gets_only_the_block() {
        assert_eq!(up("", "x\n"), block("x\n"));
    }

    #[test]
    fn body_without_trailing_newline_gets_one() {
        assert_eq!(up("", "x"), block("x\n"));
    }

    #[test]
    fn empty_body_yields_markers_only() {
        assert_eq!(up("", ""), format!("{BEGIN}\n{END}\n"));
    }

    #[test]
    fn append_after_text_ending_in_newline_leaves_one_blank_line() {
        assert_eq!(
            up("# Title\n", "x\n"),
            format!("# Title\n\n{}", block("x\n"))
        );
    }

    #[test]
    fn append_after_text_without_trailing_newline_terminates_it_first() {
        assert_eq!(up("# Title", "x\n"), format!("# Title\n\n{}", block("x\n")));
    }

    #[test]
    fn replace_in_middle_keeps_text_before_and_after_byte_identical() {
        let before = "intro\n\n";
        let after = "\nafter\n";
        let existing = format!("{before}{}{after}", block("old\n"));
        assert_eq!(
            up(&existing, "new\n"),
            format!("{before}{}{after}", block("new\n"))
        );
    }

    #[test]
    fn replace_block_at_end_without_trailing_newline() {
        let existing = format!("a\n\n{BEGIN}\nold\n{END}");
        assert_eq!(up(&existing, "new\n"), format!("a\n\n{}", block("new\n")));
    }

    #[test]
    fn upsert_twice_equals_once() {
        let once = up("# Title\n", "x\n");
        assert_eq!(up(&once, "x\n"), once);
        let once_empty = up("", "x\n");
        assert_eq!(up(&once_empty, "x\n"), once_empty);
    }

    #[test]
    fn remove_restores_original_text_after_upsert() {
        for original in ["", "# Title\n", "# Title\n\nmore\n", "a\n\n\n", "x\n\n"] {
            let installed = up(original, "managed\n");
            assert_eq!(
                rm(&installed).as_deref(),
                Some(original),
                "original {original:?}"
            );
        }
    }

    #[test]
    fn remove_without_block_is_none() {
        assert_eq!(rm("# Title\n"), None);
    }

    #[test]
    fn remove_takes_the_blank_line_before_the_block() {
        let existing = format!("a\n\n{}\nb\n", block("x\n"));
        assert_eq!(rm(&existing).as_deref(), Some("a\n\nb\n"));
    }

    #[test]
    fn remove_takes_the_blank_line_after_when_none_is_before() {
        let existing = format!("a\n{}\nb\n", block("x\n"));
        assert_eq!(rm(&existing).as_deref(), Some("a\nb\n"));
    }

    #[test]
    fn crlf_file_gets_crlf_block_and_keeps_its_bytes() {
        let original = "# Title\r\n\r\nNotes\r\n";
        let installed = up(original, "x\n");
        assert_eq!(
            installed,
            format!("{original}\r\n{BEGIN}\r\nx\r\n{END}\r\n")
        );
        assert_eq!(rm(&installed).as_deref(), Some(original));
    }

    #[test]
    fn crlf_replace_keeps_surrounding_crlf_bytes() {
        let existing = format!("# T\r\n{BEGIN}\r\nold\r\n{END}\r\nafter\r\n");
        assert_eq!(
            up(&existing, "new\n"),
            format!("# T\r\n{BEGIN}\r\nnew\r\n{END}\r\nafter\r\n")
        );
    }

    #[test]
    fn begin_without_end_is_an_error() {
        let existing = format!("x\n{BEGIN}\ny\n");
        assert_eq!(
            upsert_block(&existing, "b\n"),
            Err(MarkerError::BeginWithoutEnd)
        );
        assert_eq!(remove_block(&existing), Err(MarkerError::BeginWithoutEnd));
        assert!(has_block(&existing));
    }

    #[test]
    fn end_without_begin_is_an_error() {
        let existing = format!("x\n{END}\n");
        assert_eq!(
            upsert_block(&existing, "b\n"),
            Err(MarkerError::EndBeforeBegin)
        );
        assert_eq!(remove_block(&existing), Err(MarkerError::EndBeforeBegin));
        assert!(!has_block(&existing));
    }

    #[test]
    fn end_before_begin_is_an_error() {
        let existing = format!("{END}\n{BEGIN}\n");
        assert_eq!(
            upsert_block(&existing, "b\n"),
            Err(MarkerError::EndBeforeBegin)
        );
        assert_eq!(remove_block(&existing), Err(MarkerError::EndBeforeBegin));
    }

    #[test]
    fn two_blocks_are_an_error() {
        let existing = format!("{}\n{}", block("a\n"), block("b\n"));
        assert_eq!(
            upsert_block(&existing, "c\n"),
            Err(MarkerError::MultipleBlocks)
        );
        assert_eq!(remove_block(&existing), Err(MarkerError::MultipleBlocks));
    }

    #[test]
    fn has_block_matches_a_begin_line_with_crlf_or_padding() {
        assert!(has_block(&block("x\n")));
        assert!(has_block(&format!("a\r\n  {BEGIN}  \r\n")));
        assert!(!has_block(""));
        assert!(!has_block("mentions <!-- agora:end --> only\n"));
    }
}
