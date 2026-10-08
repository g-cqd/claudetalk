//! Expected strings were produced by Python 3.14.6 (`python3 -I -c ...`); the comment on each table says which call.

use super::*;

#[test]
fn float_repr_matches_python() {
    // repr(float(text)) for each text; repr keeps the shortest round-trip digits.
    let vectors: &[(f64, &str)] = &[
        (0.1, "0.1"),
        (0.30000000000000004, "0.30000000000000004"),
        (1e16, "1e+16"),
        (1e-5, "1e-05"),
        (1.5e16, "1.5e+16"),
        (123456789012345.0, "123456789012345.0"),
        (1e15, "1000000000000000.0"),
        (0.0001, "0.0001"),
        (-0.0, "-0.0"),
        (5e-324, "5e-324"),
        (1.7976931348623157e308, "1.7976931348623157e+308"),
        (0.0, "0.0"),
        (1.0, "1.0"),
        (2.5, "2.5"),
        (100.0, "100.0"),
        (1e-4, "0.0001"),
        (9999999999999998.0, "9999999999999998.0"),
        (1e-7, "1e-07"),
        (0.001, "0.001"),
        (0.00012345, "0.00012345"),
        (1234567.0, "1234567.0"),
        (1e22, "1e+22"),
        (1e21, "1e+21"),
        (-1.5, "-1.5"),
        (3.0, "3.0"),
        (0.5, "0.5"),
        (2.2250738585072014e-308, "2.2250738585072014e-308"),
        (4.35, "4.35"),
        (1.1, "1.1"),
        (123.456, "123.456"),
        (1e100, "1e+100"),
        (-2.5e-5, "-2.5e-05"),
        (9007199254740993.0, "9007199254740992.0"),
        (1.23456789e-5, "1.23456789e-05"),
        (12345678901234567.0, "1.2345678901234568e+16"),
    ];
    assert!(vectors.len() >= 30);
    for &(x, want) in vectors {
        assert_eq!(float_repr(x), want, "float_repr({x:?})");
    }
}

#[test]
fn float_repr_non_finite() {
    assert_eq!(float_repr(f64::NAN), "nan");
    assert_eq!(float_repr(f64::INFINITY), "inf");
    assert_eq!(float_repr(f64::NEG_INFINITY), "-inf");
}

#[test]
fn fmt_g_matches_python() {
    // "%g" % x for each x (6 significant digits, trailing zeros stripped).
    let vectors: &[(f64, &str)] = &[
        (0.0, "0"),
        (1.0, "1"),
        (2.5, "2.5"),
        (3.0, "3"),
        (0.333333, "0.333333"),
        (1234567.0, "1.23457e+06"),
        (123456.5, "123456"),
        (999999.5, "1e+06"),
        (0.0001, "0.0001"),
        (0.00001234, "1.234e-05"),
        (1.5e-5, "1.5e-05"),
        (-2.5, "-2.5"),
        (1e100, "1e+100"),
        (0.1, "0.1"),
        (100000.0, "100000"),
        (1000000.0, "1e+06"),
        (-0.0, "-0"),
        (0.5, "0.5"),
        (123456789.0, "1.23457e+08"),
        (7.0, "7"),
    ];
    for &(x, want) in vectors {
        assert_eq!(fmt_g(x), want, "fmt_g({x:?})");
    }
    assert_eq!(fmt_g(f64::NAN), "nan");
    assert_eq!(fmt_g(f64::NEG_INFINITY), "-inf");
}

#[test]
fn fmt_fixed_rounds_half_even_on_exact_value() {
    // "{:.Nf}".format(x) in Python.
    assert_eq!(fmt_fixed(0.5, 0), "0");
    assert_eq!(fmt_fixed(1.5, 0), "2");
    assert_eq!(fmt_fixed(2.5, 0), "2");
    assert_eq!(fmt_fixed(0.125, 1), "0.1");
    assert_eq!(fmt_fixed(0.125, 2), "0.12");
    assert_eq!(fmt_fixed(0.25, 1), "0.2");
    assert_eq!(fmt_fixed(0.25, 2), "0.25");
    assert_eq!(fmt_fixed(0.5, 2), "0.50");
    assert_eq!(fmt_fixed(-0.5, 0), "-0");
    assert_eq!(fmt_fixed(-0.0, 1), "-0.0");
    assert_eq!(fmt_fixed(2.675, 2), "2.67");
    assert_eq!(fmt_fixed(0.35, 1), "0.3");
    assert_eq!(fmt_fixed(1e20, 1), "100000000000000000000.0");
    assert_eq!(fmt_fixed(f64::NAN, 2), "nan");
    assert_eq!(fmt_fixed(f64::INFINITY, 2), "inf");
    assert_eq!(fmt_fixed(f64::NEG_INFINITY, 0), "-inf");
}

#[test]
fn round_half_even_matches_python() {
    // round(x, n) in Python; compared by bits so the sign of zero counts.
    let vectors: &[(f64, i32, f64)] = &[
        (0.125, 2, 0.12),
        (0.25, 1, 0.2),
        (2.675, 2, 2.67),
        (0.5, 0, 0.0),
        (1.5, 0, 2.0),
        (2.5, 0, 2.0),
        (-0.5, 0, -0.0),
        (1.25, 1, 1.2),
        (1.35, 1, 1.4),
        (-0.04, 1, -0.0),
        (0.1, 20, 0.1),
        (15.0, -1, 20.0),
        (25.0, -1, 20.0),
        (123.456, -1, 120.0),
        (1e300, -299, 1e300),
        (f64::INFINITY, 1, f64::INFINITY),
    ];
    for &(x, n, want) in vectors {
        assert_eq!(
            round_half_even(x, n).to_bits(),
            want.to_bits(),
            "round({x:?}, {n})"
        );
    }
    assert!(round_half_even(f64::NAN, 1).is_nan());
}

#[test]
fn is_printable_follows_unicode_categories() {
    let plain = ['a', ' ', 'é', '🙂', '日'];
    let not_printable = [
        '\t',
        '\u{1f}',
        '\u{7f}',
        '\u{85}',
        '\u{a0}',
        '\u{ad}',
        '\u{378}',
        '\u{200b}',
        '\u{202e}',
        '\u{2028}',
        '\u{3000}',
        '\u{e000}',
        '\u{e0001}',
        '\u{10ffff}',
    ];
    for c in plain {
        assert!(is_printable(c), "{c:?} should be printable");
    }
    for c in not_printable {
        assert!(!is_printable(c), "{c:?} should not be printable");
    }
}

#[test]
fn printable_escapes_non_printable_chars() {
    // work.py printable(): repr(ch)[1:-1] for each non-printable char; literal backslash untouched.
    let vectors: &[(&str, &str)] = &[
        ("a\tb\u{1b}c", "a\\tb\\x1bc"),
        ("\u{a0}x", "\\xa0x"),
        ("\u{202e}", "\\u202e"),
        ("\u{e0001}", "\\U000e0001"),
        ("\\x", "\\x"),
        ("é🙂", "é🙂"),
        ("\u{7f}", "\\x7f"),
        ("\u{200b}", "\\u200b"),
        ("\u{85}", "\\x85"),
        ("\u{10ffff}", "\\U0010ffff"),
        ("\u{378}", "\\u0378"),
        ("\r\n", "\\r\\n"),
    ];
    for &(input, want) in vectors {
        assert_eq!(printable(input), want, "printable({input:?})");
    }
}

#[test]
fn repr_str_quotes_and_escapes() {
    // repr(str) in Python.
    let vectors: &[(&str, &str)] = &[
        ("abc", "'abc'"),
        ("it's", "\"it's\""),
        ("say \"hi\"", "'say \"hi\"'"),
        ("both ' \"", "'both \\' \"'"),
        ("back\\slash", "'back\\\\slash'"),
        ("\u{1b}[0m", "'\\x1b[0m'"),
        ("\u{202e}", "'\\u202e'"),
        ("🙂", "'🙂'"),
        ("a\tb\nc\rd", "'a\\tb\\nc\\rd'"),
        ("\u{7f}", "'\\x7f'"),
        ("\u{85}", "'\\x85'"),
        ("\u{a0}", "'\\xa0'"),
        ("\u{e0001}", "'\\U000e0001'"),
        ("é", "'é'"),
    ];
    for &(input, want) in vectors {
        assert_eq!(repr_str(input), want, "repr_str({input:?})");
    }
}

#[test]
fn py_strip_uses_python_whitespace() {
    // str.strip() in Python: 0x1C..=0x1F and U+0085/U+00A0/U+3000 are stripped; U+200B is not whitespace.
    assert_eq!(py_strip("  a b  "), "a b");
    assert_eq!(py_strip("\u{1c}\u{1d}x\u{1e}\u{1f}"), "x");
    assert_eq!(py_strip("\u{85}y\u{a0}"), "y");
    assert_eq!(py_strip("\u{2028}z\u{3000}"), "z");
    assert_eq!(py_strip("\u{200b}q"), "\u{200b}q");
    assert_eq!(py_strip("\t\n\u{b}\u{c}\r"), "");
    assert_eq!(py_strip("   "), "");
    assert_eq!(py_strip(""), "");
}

#[test]
fn py_fsum_is_neumaier_compensated() {
    // sum(xs) in Python 3.12+ for float lists.
    let tenth = [0.1_f64; 10];
    assert_eq!(py_fsum(&tenth), 1.0);
    assert_eq!(py_fsum(&[1e100, 1.0, -1e100]), 1.0);
    assert_eq!(py_fsum(&[0.1, 0.2, 0.3]), 0.6);
    assert_eq!(py_fsum(&[1.0, 1e-16, 1e-16]), 1.0000000000000002);
    assert_eq!(py_fsum(&[3.0, 2.5]), 5.5);
    assert_eq!(py_fsum(&[]).to_bits(), 0.0_f64.to_bits());
    assert_eq!(py_fsum(&[-0.0]).to_bits(), 0.0_f64.to_bits());
    assert_eq!(py_fsum(&[f64::INFINITY, 1.0]), f64::INFINITY);
    assert_eq!(py_fsum(&[1e308, 1e308, -1e308]), f64::INFINITY);
}

#[test]
fn py_float_accepts_python_syntax() {
    // float(text) in Python.
    let finite: &[(&str, f64)] = &[
        ("1_0", 10.0),
        (" +5 ", 5.0),
        (".5", 0.5),
        ("5.", 5.0),
        ("0_1", 1.0),
        ("1_0.5_5e1_0", 105500000000.0),
        ("+.5e+1", 5.0),
        ("  -Infinity ", f64::NEG_INFINITY),
        ("inf", f64::INFINITY),
        ("1e400", f64::INFINITY),
        ("1e-400", 0.0),
    ];
    for &(text, want) in finite {
        assert_eq!(py_float(text), Some(want), "py_float({text:?})");
    }
    assert!(py_float("nan").is_some_and(f64::is_nan));
    assert!(py_float("NaN").is_some_and(f64::is_nan));
}

#[test]
fn py_float_rejects_invalid_syntax() {
    for text in [
        "1__0", "_1", "0x10", "1e", "-.e1", "+", "1.5f", "1_e5", "1e_5", "1._5", "1_", "", "   ",
        "1 2", "1_.5", "infinit",
    ] {
        assert_eq!(py_float(text), None, "py_float({text:?})");
    }
}

#[test]
fn py_int_accepts_and_rejects() {
    // int(text) in Python (base 10).
    let accepted: &[(&str, i64)] = &[
        ("1_0", 10),
        (" +5 ", 5),
        ("010", 10),
        ("-0", 0),
        ("1_000", 1000),
        (" 42\n", 42),
        ("9223372036854775807", i64::MAX),
        ("-9223372036854775808", i64::MIN),
    ];
    for &(text, want) in accepted {
        assert_eq!(py_int(text), Some(want), "py_int({text:?})");
    }
    for text in [
        "5.",
        "0x10",
        "1__0",
        "_1",
        "+",
        "9223372036854775808",
        "-9223372036854775809",
        "4 2",
        "",
        "1_",
        "1.0",
    ] {
        assert_eq!(py_int(text), None, "py_int({text:?})");
    }
}

#[test]
fn json_dumps_indent2_matches_python() {
    // json.dumps(obj, indent=2) in Python 3.14 (ensure_ascii on).
    let value = PyJson::Object(vec![
        ("name".into(), PyJson::Str("café 🙂".into())),
        ("tags".into(), PyJson::List(vec![])),
        ("meta".into(), PyJson::Object(vec![])),
        (
            "vals".into(),
            PyJson::List(vec![
                PyJson::Int(1),
                PyJson::Float(2.5),
                PyJson::Float(1e16),
                PyJson::Float(0.1),
                PyJson::Null,
                PyJson::Bool(true),
                PyJson::Bool(false),
            ]),
        ),
        (
            "nested".into(),
            PyJson::Object(vec![(
                "a".into(),
                PyJson::List(vec![PyJson::Object(vec![(
                    "x".into(),
                    PyJson::Float(-0.0),
                )])]),
            )]),
        ),
        (
            "ctl".into(),
            PyJson::Str("tab\t nl\n q\" bs\\ \u{1} \u{7f} \u{8}\u{c}\r".into()),
        ),
        (
            "inf".into(),
            PyJson::List(vec![
                PyJson::Float(f64::INFINITY),
                PyJson::Float(f64::NEG_INFINITY),
                PyJson::Float(f64::NAN),
            ]),
        ),
        ("ké".into(), PyJson::Int(0)),
    ]);
    let want = r#"{
  "name": "caf\u00e9 \ud83d\ude42",
  "tags": [],
  "meta": {},
  "vals": [
    1,
    2.5,
    1e+16,
    0.1,
    null,
    true,
    false
  ],
  "nested": {
    "a": [
      {
        "x": -0.0
      }
    ]
  },
  "ctl": "tab\t nl\n q\" bs\\ \u0001 \u007f \b\f\r",
  "inf": [
    Infinity,
    -Infinity,
    NaN
  ],
  "k\u00e9": 0
}"#;
    assert_eq!(json_dumps_indent2(&value), want);
}

#[test]
fn json_dumps_indent2_nested_empty_containers() {
    // json.dumps({"empty": [[], {}]}, indent=2) in Python.
    let value = PyJson::Object(vec![(
        "empty".into(),
        PyJson::List(vec![PyJson::List(vec![]), PyJson::Object(vec![])]),
    )]);
    let want = "{\n  \"empty\": [\n    [],\n    {}\n  ]\n}";
    assert_eq!(json_dumps_indent2(&value), want);
    assert_eq!(json_dumps_indent2(&PyJson::List(vec![])), "[]");
    assert_eq!(json_dumps_indent2(&PyJson::Float(f64::NAN)), "NaN");
}
