//! CPython `shlex` in posix mode with `punctuation_chars=";&|()<>"` and `whitespace_split=True` (shlex.py,
//! `read_token`), ported state for state, so that `sh -c` scripts split exactly as work.py splits them.
//! Comments run from `#` to the end of the line, even mid-word. An unbalanced quote or a trailing backslash is an error.

use std::str::Chars;

/// Where an escaped character returns to: a plain word, or a double-quoted string.
#[derive(Clone, Copy)]
enum Back {
    Word,
    Quote(char),
}

/// The lexer's state (shlex `self.state`): the start of a token, a word, a punctuation run, a quote, or an escape.
#[derive(Clone, Copy)]
enum Mode {
    Space,
    Word,
    Punct,
    Quote(char),
    Escape(Back),
}

const PUNCT: &str = ";&|()<>";

/// The shlex word split of `script`. `Err` where shlex raises ValueError.
pub(super) fn split(script: &str) -> Result<Vec<String>, ()> {
    let mut chars = script.chars();
    let mut pushback = None;
    let mut tokens = Vec::new();
    while let Some(token) = read_token(&mut chars, &mut pushback)? {
        tokens.push(token);
    }
    Ok(tokens)
}

fn is_punct(c: char) -> bool {
    PUNCT.contains(c)
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// shlex emits a token once it holds text or was quoted, even when empty.
fn ends_token(token: &str, quoted: bool) -> bool {
    !token.is_empty() || quoted
}

/// The token at end of input: None when nothing was read, which is shlex's end-of-input signal.
fn finish(token: String, quoted: bool) -> Option<String> {
    ends_token(&token, quoted).then_some(token)
}

/// Drops the rest of the line, as shlex's `readline` does at a comment.
fn skip_line(chars: &mut Chars<'_>) {
    for c in chars.by_ref() {
        if c == '\n' {
            break;
        }
    }
}

/// One token, or None at end of input. `pushback` holds the one character shlex returns to the stream.
fn read_token(chars: &mut Chars<'_>, pushback: &mut Option<char>) -> Result<Option<String>, ()> {
    let mut token = String::new();
    let mut quoted = false;
    let mut mode = Mode::Space;
    loop {
        let next = pushback.take().or_else(|| chars.next());
        match mode {
            Mode::Space => {
                let Some(c) = next else {
                    return Ok(finish(token, quoted));
                };
                if is_space(c) {
                    if ends_token(&token, quoted) {
                        return Ok(Some(token));
                    }
                } else if c == '#' {
                    skip_line(chars);
                } else if c == '\\' {
                    mode = Mode::Escape(Back::Word);
                } else if is_punct(c) {
                    token.push(c);
                    mode = Mode::Punct;
                } else if c == '\'' || c == '"' {
                    quoted = true;
                    mode = Mode::Quote(c);
                } else {
                    token.push(c);
                    mode = Mode::Word;
                }
            }
            Mode::Word => {
                let Some(c) = next else {
                    return Ok(finish(token, quoted));
                };
                if is_space(c) || c == '#' {
                    if c == '#' {
                        skip_line(chars);
                    }
                    mode = Mode::Space;
                    if ends_token(&token, quoted) {
                        return Ok(Some(token));
                    }
                } else if c == '\'' || c == '"' {
                    quoted = true;
                    mode = Mode::Quote(c);
                } else if c == '\\' {
                    mode = Mode::Escape(Back::Word);
                } else if is_punct(c) {
                    *pushback = Some(c);
                    mode = Mode::Space;
                    if ends_token(&token, quoted) {
                        return Ok(Some(token));
                    }
                } else {
                    token.push(c);
                }
            }
            Mode::Punct => {
                let Some(c) = next else {
                    return Ok(finish(token, quoted));
                };
                if is_space(c) || c == '#' {
                    if c == '#' {
                        skip_line(chars);
                    }
                    mode = Mode::Space;
                    if ends_token(&token, quoted) {
                        return Ok(Some(token));
                    }
                } else if is_punct(c) {
                    token.push(c);
                } else {
                    *pushback = Some(c);
                    return Ok(finish(token, quoted));
                }
            }
            Mode::Quote(q) => {
                let Some(c) = next else {
                    return Err(());
                };
                if c == q {
                    mode = Mode::Word;
                } else if c == '\\' && q == '"' {
                    mode = Mode::Escape(Back::Quote(q));
                } else {
                    token.push(c);
                }
            }
            Mode::Escape(back) => {
                let Some(c) = next else {
                    return Err(());
                };
                // Inside double quotes only \\ and \" are escapes; any other backslash stays.
                if matches!(back, Back::Quote(q) if c != '\\' && c != q) {
                    token.push('\\');
                }
                token.push(c);
                mode = match back {
                    Back::Word => Mode::Word,
                    Back::Quote(q) => Mode::Quote(q),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::split;

    fn words(script: &str) -> Vec<String> {
        split(script).expect("the script lexes")
    }

    #[test]
    fn comment_runs_to_end_of_line_even_mid_word() {
        assert_eq!(words("a#comment"), ["a"]);
        assert_eq!(words("a;#c\nb"), ["a", ";", "b"]);
    }

    #[test]
    fn a_newline_inside_an_escape_is_a_word_character() {
        assert_eq!(words("echo \\\n"), ["echo", "\n"]);
    }

    #[test]
    fn only_space_tab_cr_and_lf_separate_words() {
        assert_eq!(words("a\u{a0}b é"), ["a\u{a0}b", "é"]);
    }

    #[test]
    fn unbalanced_quote_or_trailing_backslash_is_an_error() {
        assert!(split("echo \"unterminated").is_err());
        assert!(split("echo 'abc").is_err());
        assert!(split("echo \"abc\\\"").is_err());
        assert!(split("echo \\").is_err());
    }

    #[test]
    fn empty_quotes_are_an_empty_word() {
        assert_eq!(words("echo ''  x"), ["echo", "", "x"]);
    }
}
