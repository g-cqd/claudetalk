//! work.py text helpers (ref-queue-cli section 0, ref-admission "Format helpers").

use crate::pyfmt::{fmt_fixed, fmt_g, printable, round_half_even};

/// Python `span`: "-", "{s:.1f} s", "{m} min {s} s", or "{h} h {m} min". Non-finite input gives "-" (Python raises).
pub fn span(seconds: Option<f64>) -> String {
    let Some(s) = seconds.filter(|s| s.is_finite()) else {
        return "-".to_owned();
    };
    if s < 60.0 {
        return format!("{} s", fmt_fixed(s, 1));
    }
    if s < 3600.0 {
        let minutes = py_floordiv(s, 60.0).trunc();
        let seconds = (s % 60.0).trunc();
        return format!("{} min {} s", fmt_fixed(minutes, 0), fmt_fixed(seconds, 0));
    }
    let hours = py_floordiv(s, 3600.0).trunc();
    let minutes = py_floordiv(s % 3600.0, 60.0).trunc();
    format!("{} h {} min", fmt_fixed(hours, 0), fmt_fixed(minutes, 0))
}

/// Python `memory`: "-", "{kib/1024:.0f} MiB" below 10 GiB, else "{kib/1024/1024:.1f} GiB".
pub fn memory(kib: Option<u64>) -> String {
    let Some(kib) = kib else {
        return "-".to_owned();
    };
    if kib < 10 * 1024 * 1024 {
        format!("{} MiB", fmt_fixed(kib_as_f64(kib) / 1024.0, 0))
    } else {
        gib(kib)
    }
}

/// Python `gib`: "{kib/1024/1024:.1f} GiB".
pub fn gib(kib: u64) -> String {
    format!("{} GiB", fmt_fixed(kib_as_f64(kib) / 1024.0 / 1024.0, 1))
}

/// fmt_g(round_half_even(v, 1)).
pub fn number(v: f64) -> String {
    fmt_g(round_half_even(v, 1))
}

/// "1 core" when the rounded number is exactly "1", otherwise "{number} cores".
pub fn cores_text(v: f64) -> String {
    let n = number(v);
    if n == "1" {
        format!("{n} core")
    } else {
        format!("{n} cores")
    }
}

/// Python `capped`: "label: a, b" up to `limit` items, else "label (the first {limit} of {n}; all of them with
/// --json): first {limit} joined".
pub fn capped(label: &str, items: &[String], limit: usize) -> String {
    if items.len() <= limit {
        return format!("{label}: {}", items.join(", "));
    }
    let shown: Vec<&str> = items.iter().take(limit).map(String::as_str).collect();
    format!(
        "{label} (the first {limit} of {}; all of them with --json): {}",
        items.len(),
        shown.join(", ")
    )
}

/// shlex.quote of one word: "''" for empty, unchanged when every char is in [A-Za-z0-9@%+=:,./_-], else single
/// quoted with each ' written as '"'"'.
pub fn shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_owned();
    }
    let safe = |c: char| c.is_ascii_alphanumeric() || "@%+=:,./_-".contains(c);
    if s.chars().all(safe) {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// printable(shlex.join(argv)): the command as one line, control and invisible characters escaped.
pub fn display(argv: &[String]) -> String {
    let joined: Vec<String> = argv.iter().map(|arg| shlex_quote(arg)).collect();
    printable(&joined.join(" "))
}

/// The display text when it has at most `limit` chars; otherwise its first `limit - 1` chars followed by "…".
pub fn brief(argv: &[String], limit: usize) -> String {
    let text = display(argv);
    if text.chars().count() <= limit {
        return text;
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// "a", "a and b", "a, b and c" (Python enumerate_ids in work.py).
pub fn enumerate_ids(ids: &[String]) -> String {
    match ids {
        [] => String::new(),
        [only] => only.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// Python float `//` for positive operands: the floor of the quotient, rounded up when it sits just above an integer.
fn py_floordiv(x: f64, y: f64) -> f64 {
    let rem = x % y;
    let quotient = (x - rem) / y;
    let floor = quotient.floor();
    if quotient - floor > 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

/// Exact conversion for display arithmetic; KiB counts stay far below 2^53.
fn kib_as_f64(kib: u64) -> f64 {
    kib as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn span_formats_each_range() {
        assert_eq!(span(None), "-");
        assert_eq!(span(Some(0.04)), "0.0 s");
        assert_eq!(span(Some(2.5)), "2.5 s");
        assert_eq!(span(Some(59.96)), "60.0 s");
        assert_eq!(span(Some(61.0)), "1 min 1 s");
        assert_eq!(span(Some(300.0)), "5 min 0 s");
        assert_eq!(span(Some(3599.9)), "59 min 59 s");
        assert_eq!(span(Some(3600.0)), "1 h 0 min");
        assert_eq!(span(Some(7260.0)), "2 h 1 min");
        assert_eq!(span(Some(f64::INFINITY)), "-");
    }

    #[test]
    fn memory_switches_unit_at_ten_gib() {
        assert_eq!(memory(None), "-");
        assert_eq!(memory(Some(1024 * 1024)), "1024 MiB");
        assert_eq!(memory(Some(10 * 1024 * 1024 - 1)), "10240 MiB");
        assert_eq!(memory(Some(10 * 1024 * 1024)), "10.0 GiB");
    }

    #[test]
    fn gib_has_one_decimal() {
        assert_eq!(gib(0), "0.0 GiB");
        assert_eq!(gib(1_572_864), "1.5 GiB");
        assert_eq!(gib(14_680_064), "14.0 GiB");
    }

    #[test]
    fn number_rounds_to_one_decimal_then_formats_g() {
        assert_eq!(number(0.25), "0.2");
        assert_eq!(number(0.35), "0.3");
        assert_eq!(number(7.4), "7.4");
        assert_eq!(number(8.0), "8");
        assert_eq!(number(2.5), "2.5");
        assert_eq!(number(1_234_567.0), "1.23457e+06");
    }

    #[test]
    fn cores_text_picks_singular_only_for_one() {
        assert_eq!(cores_text(1.0), "1 core");
        assert_eq!(cores_text(0.96), "1 core");
        assert_eq!(cores_text(0.5), "0.5 cores");
        assert_eq!(cores_text(8.0), "8 cores");
        assert_eq!(cores_text(0.0), "0 cores");
    }

    #[test]
    fn capped_lists_all_items_up_to_the_limit() {
        let ten: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        assert_eq!(
            capped("after", &ten, 10),
            "after: 0, 1, 2, 3, 4, 5, 6, 7, 8, 9"
        );
        assert_eq!(capped("after", &[], 10), "after: ");
    }

    #[test]
    fn capped_cuts_to_the_limit_and_counts_all() {
        let eleven: Vec<String> = (0..11).map(|i| i.to_string()).collect();
        assert_eq!(
            capped("files", &eleven, 10),
            "files (the first 10 of 11; all of them with --json): 0, 1, 2, 3, 4, 5, 6, 7, 8, 9"
        );
        let thousand: Vec<String> = (0..1000).map(|i| i.to_string()).collect();
        let text = capped("files", &thousand, 10);
        assert!(text.starts_with("files (the first 10 of 1000; all of them with --json): 0, 1, "));
        assert!(text.ends_with(", 8, 9"));
    }

    #[test]
    fn shlex_quote_matches_python() {
        assert_eq!(shlex_quote(""), "''");
        assert_eq!(shlex_quote("rm -rf x"), "'rm -rf x'");
        assert_eq!(shlex_quote("a'b"), "'a'\"'\"'b'");
        assert_eq!(
            shlex_quote("plain-arg_1.2/x@y:z,%+="),
            "plain-arg_1.2/x@y:z,%+="
        );
    }

    #[test]
    fn display_escapes_control_characters() {
        assert_eq!(display(&argv(&["echo", "a b"])), "echo 'a b'");
        assert_eq!(display(&argv(&["printf", "\x1b[0m"])), "printf '\\x1b[0m'");
        assert_eq!(
            display(&argv(&["echo", "a\tb", "\u{202e}"])),
            "echo 'a\\tb' '\\u202e'"
        );
        assert_eq!(display(&[]), "");
    }

    #[test]
    fn brief_keeps_short_text_and_cuts_long_text_with_ellipsis() {
        let exact = vec!["x".repeat(200)];
        assert_eq!(brief(&exact, 200), "x".repeat(200));
        let long = vec!["x".repeat(300)];
        let cut = brief(&long, 200);
        assert_eq!(cut.chars().count(), 200);
        assert_eq!(cut, format!("{}…", "x".repeat(199)));
    }

    #[test]
    fn enumerate_ids_joins_with_and() {
        assert_eq!(enumerate_ids(&[]), "");
        assert_eq!(enumerate_ids(&argv(&["a"])), "a");
        assert_eq!(enumerate_ids(&argv(&["a", "b"])), "a and b");
        assert_eq!(enumerate_ids(&argv(&["a", "b", "c"])), "a, b and c");
    }
}
