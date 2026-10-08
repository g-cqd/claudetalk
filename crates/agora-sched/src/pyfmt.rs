//! Python-compatible formatting and parsing helpers (json.dumps, float repr, %g, round, str.isprintable, ...).

mod printable_table;
#[cfg(test)]
mod tests;

use std::cmp::Ordering;

use printable_table::NON_PRINTABLE;

#[derive(Debug, Clone, PartialEq)]
pub enum PyJson {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<PyJson>),
    Object(Vec<(String, PyJson)>),
}

/// json.dumps(v, indent=2): 2-space indent, "key": value, ensure_ascii (every char outside 0x20..=0x7e escaped; short
/// escapes \" \\ \n \r \t \b \f; others \u00xx lowercase; astral as surrogate pairs), [] / {} when empty,
/// floats via float_repr (inf -> Infinity, -inf -> -Infinity, nan -> NaN). No trailing newline.
pub fn json_dumps_indent2(v: &PyJson) -> String {
    let mut out = String::new();
    write_json(v, 0, &mut out);
    out
}

/// Python float repr: shortest round-trip digits; fixed notation when the decimal exponent is in [-4, 16), integral
/// values get ".0"; else "1e-05", "1.5e+16", "1e+16" (mantissa without ".0", exponent sign + at least 2 digits).
pub fn float_repr(x: f64) -> String {
    if let Some(text) = non_finite_text(x) {
        return text.to_owned();
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    let (digits, exp) = shortest_digits(x.abs());
    let body = if (-4..16).contains(&exp) {
        fixed_digits(&digits, exp)
    } else {
        sci_digits(&digits, exp)
    };
    format!("{sign}{body}")
}

/// Python "%g" / "{:g}": 6 significant digits, trailing zeros stripped, scientific when exp < -4 or >= 6 ("1.23457e+06").
pub fn fmt_g(x: f64) -> String {
    if let Some(text) = non_finite_text(x) {
        return text.to_owned();
    }
    // The exponent comes from the rounded 6-digit form, as C's %g specifies, so 999999.5 becomes 1e+06.
    let text = format!("{x:.5e}");
    let (mantissa, exp) = split_exp(&text);
    if (-4..6).contains(&exp) {
        let decimals = usize::try_from(5 - exp).unwrap_or(0);
        strip_zeros(&format!("{x:.decimals$}"))
    } else {
        format!("{}{}", strip_zeros(mantissa), exp_suffix(exp))
    }
}

/// Python "{:.Nf}" (exact binary value, round-half-even on ties).
pub fn fmt_fixed(x: f64, decimals: usize) -> String {
    if let Some(text) = non_finite_text(x) {
        return text.to_owned();
    }
    format!("{x:.decimals$}")
}

/// Python round(x, ndigits) (exact value, half-even).
pub fn round_half_even(x: f64, ndigits: i32) -> f64 {
    if !x.is_finite() {
        return x;
    }
    if ndigits >= 0 {
        let places = usize::try_from(ndigits).unwrap_or(usize::MAX);
        // Every finite f64 has at most 1074 fractional decimal digits, so rounding further is the identity.
        if places >= 1100 {
            return x;
        }
        format!("{x:.places$}").parse::<f64>().unwrap_or(x)
    } else {
        round_to_power_of_ten(
            x,
            usize::try_from(ndigits.unsigned_abs()).unwrap_or(usize::MAX),
        )
    }
}

/// Python 3.14 str.isprintable() for one char: false for categories Cc Cf Cs Co Cn Zl Zp Zs except U+0020.
/// Implemented with a static sorted range table GENERATED from Python 3.14 (script
/// /tmp/agora-m5/fixgen/gen_printable.py, written by the pyfmt task; the table lives in pyfmt/printable_table.rs).
pub fn is_printable(c: char) -> bool {
    let code = u32::from(c);
    NON_PRINTABLE
        .binary_search_by(|&(lo, hi)| {
            if hi < code {
                Ordering::Less
            } else if lo > code {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_err()
}

/// work.py printable(): each non-printable char -> repr(char)[1:-1] (\t \n \r \xNN \uNNNN \UNNNNNNNN, lowercase hex).
pub fn printable(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if is_printable(c) {
            out.push(c);
        } else {
            push_escape(c, &mut out);
        }
    }
    out
}

/// Python repr(str) (quote choice, escapes as above).
pub fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        if c == quote {
            out.push('\\');
            out.push(c);
        } else if c != '\\' && is_printable(c) {
            out.push(c);
        } else {
            push_escape(c, &mut out);
        }
    }
    out.push(quote);
    out
}

/// Python str.strip() (Unicode whitespace incl. U+001C..U+001F, U+0085, U+00A0, U+1680, U+2000..U+200A, U+2028,
/// U+2029, U+202F, U+205F, U+3000).
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// Python >= 3.12 sum() over floats: Neumaier compensated, seed = 0.0 then each element (see ref-ordering section 9).
pub fn py_fsum(xs: &[f64]) -> f64 {
    let mut hi = 0.0_f64;
    let mut lo = 0.0_f64;
    for &x in xs {
        let t = hi + x;
        if hi.abs() >= x.abs() {
            lo += (hi - t) + x;
        } else {
            lo += (x - t) + hi;
        }
        hi = t;
    }
    if lo != 0.0 && lo.is_finite() {
        hi + lo
    } else {
        hi
    }
}

/// Python float(text) parser: surrounding py whitespace, sign, "_" between digits, inf/infinity/nan (case-insens.),
/// exponents, ".5", "5."; None if invalid.
pub fn py_float(text: &str) -> Option<f64> {
    let body = py_strip(text);
    if !underscores_ok(body) {
        return None;
    }
    // Rust's parser takes the same grammar once the separators are gone (sign, digits, '.', exponent, inf/nan).
    let cleaned: String = body.chars().filter(|&c| c != '_').collect();
    cleaned.parse::<f64>().ok()
}

/// Python int(text) parser (whitespace, sign, "_" between digits); None if invalid or out of i64.
pub fn py_int(text: &str) -> Option<i64> {
    let body = py_strip(text);
    if !underscores_ok(body) {
        return None;
    }
    let cleaned: String = body.chars().filter(|&c| c != '_').collect();
    cleaned.parse::<i64>().ok()
}

fn write_json(v: &PyJson, depth: usize, out: &mut String) {
    match v {
        PyJson::Null => out.push_str("null"),
        PyJson::Bool(true) => out.push_str("true"),
        PyJson::Bool(false) => out.push_str("false"),
        PyJson::Int(i) => out.push_str(&i.to_string()),
        PyJson::Float(f) => out.push_str(&json_float(*f)),
        PyJson::Str(s) => write_json_str(s, out),
        PyJson::List(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                push_separator(i, depth, out);
                write_json(item, depth + 1, out);
            }
            close_block(depth, ']', out);
        }
        PyJson::Object(members) => {
            if members.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (key, value)) in members.iter().enumerate() {
                push_separator(i, depth, out);
                write_json_str(key, out);
                out.push_str(": ");
                write_json(value, depth + 1, out);
            }
            close_block(depth, '}', out);
        }
    }
}

fn json_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_owned()
    } else if f == f64::INFINITY {
        "Infinity".to_owned()
    } else if f == f64::NEG_INFINITY {
        "-Infinity".to_owned()
    } else {
        float_repr(f)
    }
}

fn push_separator(index: usize, depth: usize, out: &mut String) {
    if index > 0 {
        out.push(',');
    }
    out.push('\n');
    push_indent(depth + 1, out);
}

fn close_block(depth: usize, close: char, out: &mut String) {
    out.push('\n');
    push_indent(depth, out);
    out.push(close);
}

fn push_indent(depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// json.dumps string escaping with ensure_ascii: every char outside 0x20..=0x7e is escaped (DEL included).
fn write_json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
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
                let mut units = [0_u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// Python repr() escape for one non-printable char (or a backslash or quote inside repr_str): the text between the
/// quotes of repr(c), lowercase hex.
fn push_escape(c: char, out: &mut String) {
    match c {
        '\\' => out.push_str("\\\\"),
        '\t' => out.push_str("\\t"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        _ => {
            let code = u32::from(c);
            if code < 0x100 {
                out.push_str(&format!("\\x{code:02x}"));
            } else if code < 0x1_0000 {
                out.push_str(&format!("\\u{code:04x}"));
            } else {
                out.push_str(&format!("\\U{code:08x}"));
            }
        }
    }
}

/// Python str.isspace() for the characters str.strip() removes.
fn is_py_space(c: char) -> bool {
    matches!(
        c,
        '\t'..='\r'
            | '\u{1c}'..='\u{20}'
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

/// Python number syntax allows "_" only between two digits.
fn underscores_ok(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digit_at = |i: Option<usize>| i.and_then(|i| bytes.get(i)).is_some_and(u8::is_ascii_digit);
    bytes
        .iter()
        .enumerate()
        .all(|(i, &b)| b != b'_' || (digit_at(i.checked_sub(1)) && digit_at(i.checked_add(1))))
}

/// "nan", "inf" and "-inf" as Python prints them in repr, %g and {:f}; None for finite values.
fn non_finite_text(x: f64) -> Option<&'static str> {
    if x.is_nan() {
        Some("nan")
    } else if x == f64::INFINITY {
        Some("inf")
    } else if x == f64::NEG_INFINITY {
        Some("-inf")
    } else {
        None
    }
}

/// Shortest round-trip digits of a finite, non-negative x (trailing zeros dropped), with the decimal exponent of the
/// first digit. Rust's `{:e}` finds the shortest length but resolves an exact tie between two equally short candidates
/// upwards; Python's repr takes the nearest candidate and resolves an exact tie half-even. So when the correctly
/// rounded candidate of that length round-trips, it is the one used.
fn shortest_digits(x: f64) -> (String, i32) {
    let shortest = format!("{x:e}");
    let (mantissa, exp) = split_exp(&shortest);
    let len = mantissa.chars().filter(char::is_ascii_digit).count();
    let nearest = format!("{:.*e}", len.saturating_sub(1), x);
    let (text, exp) = if nearest.parse::<f64>().is_ok_and(|v| v == x) {
        split_exp(&nearest)
    } else {
        (mantissa, exp)
    };
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    let trimmed = digits.trim_end_matches('0');
    if trimmed.is_empty() {
        ("0".to_owned(), 0)
    } else {
        (trimmed.to_owned(), exp)
    }
}

/// Splits Rust's `{:e}` text ("1.5e-7") into the mantissa and the decimal exponent.
fn split_exp(text: &str) -> (&str, i32) {
    match text.split_once('e') {
        Some((mantissa, exp)) => (mantissa, exp.parse().unwrap_or(0)),
        None => (text, 0),
    }
}

/// Digits d1d2... with the first digit at 10^exp, written in fixed notation (repr rules: integral values get ".0").
fn fixed_digits(digits: &str, exp: i32) -> String {
    if exp < 0 {
        return format!(
            "0.{}{digits}",
            "0".repeat(usize::try_from(-exp - 1).unwrap_or(0))
        );
    }
    let int_len = usize::try_from(exp + 1).unwrap_or(0);
    match digits.split_at_checked(int_len) {
        Some((int, frac)) if !frac.is_empty() => format!("{int}.{frac}"),
        _ => format!(
            "{digits}{}.0",
            "0".repeat(int_len.saturating_sub(digits.len()))
        ),
    }
}

/// Digits in scientific notation as repr writes them: "1e+16", "1.5e-05" (no ".0" on a single digit).
fn sci_digits(digits: &str, exp: i32) -> String {
    let mantissa = match digits.split_at_checked(1) {
        Some((head, tail)) if !tail.is_empty() => format!("{head}.{tail}"),
        _ => digits.to_owned(),
    };
    format!("{mantissa}{}", exp_suffix(exp))
}

/// "e+16", "e-05": a sign and at least two exponent digits.
fn exp_suffix(exp: i32) -> String {
    format!(
        "e{}{:02}",
        if exp < 0 { '-' } else { '+' },
        exp.unsigned_abs()
    )
}

/// %g trailing-zero rule: strip zeros after the point, then the point itself. Text without a point is unchanged.
fn strip_zeros(text: &str) -> String {
    if !text.contains('.') {
        return text.to_owned();
    }
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// round(x, -k): exact decimal digits of |x|, half-even at the 10^k place, then parsed back.
fn round_to_power_of_ten(x: f64, k: usize) -> f64 {
    let zero = if x.is_sign_negative() { -0.0 } else { 0.0 };
    let text = format!("{:.1100}", x.abs());
    let (int_part, frac_part) = text.split_once('.').unwrap_or((text.as_str(), ""));
    let Some(keep) = int_part.len().checked_sub(k) else {
        return zero;
    };
    let Some((head, int_tail)) = int_part.split_at_checked(keep) else {
        return zero;
    };
    let rest: String = int_tail.chars().chain(frac_part.chars()).collect();
    let mut rest_chars = rest.chars();
    let first = rest_chars.next().unwrap_or('0');
    let beyond_half = rest_chars.any(|c| c != '0');
    let last_odd = head
        .chars()
        .last()
        .is_some_and(|c| matches!(c, '1' | '3' | '5' | '7' | '9'));
    let round_up = first > '5' || (first == '5' && (beyond_half || last_odd));
    let kept = if round_up {
        increment_decimal(head)
    } else {
        head.to_owned()
    };
    let digits = if kept.is_empty() {
        "0".to_owned()
    } else {
        kept
    };
    let magnitude: f64 = format!("{digits}{}", "0".repeat(k)).parse().unwrap_or(0.0);
    if x.is_sign_negative() {
        -magnitude
    } else {
        magnitude
    }
}

/// Adds one to a string of ASCII decimal digits ("" gives "1", "99" gives "100").
fn increment_decimal(digits: &str) -> String {
    let mut bytes = digits.as_bytes().to_vec();
    let mut carry = true;
    for b in bytes.iter_mut().rev() {
        if !carry {
            break;
        }
        if *b == b'9' {
            *b = b'0';
        } else {
            *b += 1;
            carry = false;
        }
    }
    if carry {
        bytes.insert(0, b'1');
    }
    bytes.into_iter().map(char::from).collect()
}
