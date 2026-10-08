//! Environment scrubbing for `work.run` (D14).

use std::collections::BTreeMap;

/// At most this many variables survive a scrub.
const MAX_VARS: usize = 512;
/// Values longer than this many bytes are dropped (32 KiB).
const MAX_VALUE_BYTES: usize = 32 * 1024;
/// The surviving names and values together may not exceed this many bytes (256 KiB).
const MAX_TOTAL_BYTES: usize = 256 * 1024;

/// Name fragments that mark a secret, matched case-insensitively anywhere in the name.
const SECRET_FRAGMENTS: [&str; 13] = [
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PASSPHRASE",
    "CREDENTIAL",
    "API_KEY",
    "APIKEY",
    "ACCESS_KEY",
    "PRIVATE",
    "COOKIE",
    "SESSION",
    "WEBHOOK",
];

/// Name suffixes that mark a secret (keys, DSNs, auth and personal access tokens), matched case-insensitively.
const SECRET_SUFFIXES: [&str; 4] = ["_KEY", "_DSN", "_AUTH", "_PAT"];

/// Name prefixes that are dropped, matched case-insensitively. `LD_` covers every dynamic-loader variable.
const SECRET_PREFIXES: [&str; 3] = ["AGORA_", "DYLD_", "LD_"];

/// Exact names that are dropped, matched case-insensitively.
const SECRET_NAMES: [&str; 3] = ["SSH_AUTH_SOCK", "GPG_AGENT_INFO", "DATABASE_URL"];

/// True when a variable with this name must not be passed to a job's command.
pub fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_FRAGMENTS
        .iter()
        .any(|fragment| upper.contains(fragment))
        || SECRET_SUFFIXES.iter().any(|suffix| upper.ends_with(suffix))
        || SECRET_PREFIXES
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        || SECRET_NAMES.contains(&upper.as_str())
}

/// `[A-Za-z_][A-Za-z0-9_]*`, ASCII only.
fn is_valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Keeps only the variables a job may inherit: valid names that are not secrets, and values that are at most 32 KiB
/// and contain no NUL. A later duplicate of a kept name replaces the earlier one; a rejected duplicate changes
/// nothing. The result holds the first 512 names in sorted order, and stops at the first one whose name and value
/// would take the total of names and values past 256 KiB (later names are not considered, even smaller ones).
pub fn scrub(vars: impl IntoIterator<Item = (String, String)>) -> BTreeMap<String, String> {
    let mut kept = BTreeMap::new();
    for (name, value) in vars {
        if is_valid_name(&name)
            && !is_secret_name(&name)
            && value.len() <= MAX_VALUE_BYTES
            && !value.contains('\0')
        {
            kept.insert(name, value);
        }
    }
    let mut total = 0_usize;
    let mut out = BTreeMap::new();
    for (name, value) in kept.into_iter().take(MAX_VARS) {
        let size = name.len().saturating_add(value.len());
        if total.saturating_add(size) > MAX_TOTAL_BYTES {
            break;
        }
        total += size;
        out.insert(name, value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn secret_fragments_are_dropped() {
        let out = scrub(pairs(&[
            ("GITHUB_TOKEN", "t"),
            ("OPENAI_API_KEY", "k"),
            ("DB_PASSWORD", "p"),
            ("MY_SECRET_VALUE", "s"),
            ("OLD_PASSWD", "p"),
            ("AWS_CREDENTIAL_FILE", "c"),
            ("MY_APIKEY", "k"),
            ("PRIVATE_KEY_PATH", "x"),
            ("SESSION_ID", "x"),
            ("COOKIE_JAR", "x"),
        ]));
        assert!(out.is_empty(), "unexpected survivors: {out:?}");
    }

    #[test]
    fn lowercase_secret_names_are_dropped() {
        let out = scrub(pairs(&[
            ("aws_secret_access_key", "x"),
            ("github_token", "x"),
            ("openai_api_key", "x"),
            ("agora_home", "/x"),
            ("dyld_insert_libraries", "/x"),
        ]));
        assert!(out.is_empty(), "unexpected survivors: {out:?}");
    }

    #[test]
    fn agora_and_dyld_prefixes_are_dropped() {
        let out = scrub(pairs(&[
            ("AGORA_HOME", "/h"),
            ("AGORA_FOLDER", "/f"),
            ("DYLD_INSERT_LIBRARIES", "/lib.dylib"),
            ("DYLD_LIBRARY_PATH", "/lib"),
        ]));
        assert!(out.is_empty(), "unexpected survivors: {out:?}");
    }

    #[test]
    fn named_loader_and_agent_variables_are_dropped() {
        let out = scrub(pairs(&[
            ("SSH_AUTH_SOCK", "/s"),
            ("GPG_AGENT_INFO", "/g"),
            ("LD_PRELOAD", "/p.so"),
            ("LD_LIBRARY_PATH", "/l"),
            ("LD_AUDIT", "/a.so"),
        ]));
        assert!(out.is_empty(), "unexpected survivors: {out:?}");
    }

    #[test]
    fn common_variables_are_kept_with_values() {
        let out = scrub(pairs(&[
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/Users/x"),
            ("CARGO_TARGET_DIR", "/t"),
            ("LANG", "C.UTF-8"),
            ("LC_ALL", "C"),
            ("LDFLAGS", "-L/usr/lib"),
        ]));
        assert_eq!(out.len(), 6);
        assert_eq!(out.get("PATH").map(String::as_str), Some("/usr/bin:/bin"));
        assert_eq!(out.get("LC_ALL").map(String::as_str), Some("C"));
        assert_eq!(out.get("LDFLAGS").map(String::as_str), Some("-L/usr/lib"));
    }

    #[test]
    fn is_secret_name_matches_the_rule() {
        assert!(is_secret_name("GITHUB_TOKEN"));
        assert!(is_secret_name("aws_secret_access_key"));
        assert!(is_secret_name("AGORA_HOME"));
        assert!(is_secret_name("DYLD_"));
        assert!(is_secret_name("LD_AUDIT"));
        assert!(!is_secret_name("PATH"));
        assert!(!is_secret_name("LDFLAGS"));
        assert!(!is_secret_name("AGORA"));
    }

    #[test]
    fn invalid_names_are_dropped() {
        let out = scrub(pairs(&[
            ("", "x"),
            ("1ABC", "x"),
            ("MY-VAR", "x"),
            ("A.B", "x"),
            ("NAME=X", "x"),
            ("A B", "x"),
            ("CAFÉ", "x"),
            ("_OK", "kept"),
            ("OK2", "kept"),
        ]));
        let names: Vec<&str> = out.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["OK2", "_OK"]);
    }

    #[test]
    fn values_with_nul_are_dropped() {
        let out = scrub(pairs(&[("BAD", "a\0b"), ("GOOD", "ab")]));
        assert_eq!(out.len(), 1);
        assert!(out.contains_key("GOOD"));
    }

    #[test]
    fn value_longer_than_32_kib_is_dropped() {
        let at_limit = "x".repeat(MAX_VALUE_BYTES);
        let over_limit = "x".repeat(MAX_VALUE_BYTES + 1);
        let out = scrub(vec![
            ("AT_LIMIT".to_string(), at_limit.clone()),
            ("OVER_LIMIT".to_string(), over_limit),
        ]);
        assert_eq!(out.get("AT_LIMIT"), Some(&at_limit));
        assert!(!out.contains_key("OVER_LIMIT"));
    }

    #[test]
    fn later_duplicate_overwrites_earlier_one() {
        let out = scrub(pairs(&[("PATH", "/a"), ("PATH", "/b")]));
        assert_eq!(out.get("PATH").map(String::as_str), Some("/b"));
    }

    #[test]
    fn rejected_duplicate_does_not_overwrite() {
        let out = scrub(pairs(&[("LANG", "C.UTF-8"), ("LANG", "x\0y")]));
        assert_eq!(out.get("LANG").map(String::as_str), Some("C.UTF-8"));
    }

    #[test]
    fn keeps_at_most_512_names_in_sorted_order_after_filtering() {
        // 100 secrets sort before the ordinary names, so the cap must count only the survivors.
        let mut input: Vec<(String, String)> = (0..100)
            .map(|i| (format!("AAA_TOKEN_{i:03}"), "s".to_string()))
            .collect();
        input.extend((0..600).map(|i| (format!("V{i:03}"), "v".to_string())));
        let out = scrub(input);
        assert_eq!(out.len(), MAX_VARS);
        assert!(out.contains_key("V000"));
        assert!(out.contains_key("V511"));
        assert!(!out.contains_key("V512"));
        assert!(out.keys().all(|k| k.starts_with('V')));
    }

    #[test]
    fn added_secret_names_are_dropped() {
        let out = scrub(pairs(&[
            ("STRIPE_KEY", "k"),
            ("AWS_ACCESS_KEY_ID", "k"),
            ("DATABASE_URL", "postgres://u:p@h/db"),
            ("SENTRY_DSN", "https://k@o.ingest.sentry.io/1"),
            ("GH_PAT", "p"),
            ("SLACK_WEBHOOK_URL", "https://hooks.slack.com/x"),
            ("MY_PASSPHRASE", "p"),
            ("GITLAB_AUTH", "a"),
            ("aws_access_key_id", "k"),
            ("stripe_key", "k"),
        ]));
        assert!(out.is_empty(), "unexpected survivors: {out:?}");
    }

    #[test]
    fn every_ld_name_is_dropped_and_ldflags_is_kept() {
        let out = scrub(pairs(&[
            ("LD_DEBUG", "all"),
            ("ld_debug_output", "/tmp/x"),
            ("LD_ANYTHING_NEW", "1"),
            ("LDFLAGS", "-L/usr/lib"),
        ]));
        assert_eq!(
            out.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["LDFLAGS"]
        );
    }

    #[test]
    fn look_alike_names_are_kept() {
        let out = scrub(pairs(&[
            ("KEYBOARD_LAYOUT", "us"),
            ("DATABASE_HOST", "localhost"),
            ("MY_PATH", "/x"),
        ]));
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn total_bytes_at_the_cap_are_kept_and_the_next_name_is_not() {
        // Eight names of 8 bytes and values of 32760 bytes make exactly 256 KiB.
        let mut input: Vec<(String, String)> = (0..8)
            .map(|i| (format!("VAR_000{i}"), "x".repeat(32_760)))
            .collect();
        input.push(("VAR_0008".to_string(), "y".to_string()));
        let out = scrub(input);
        assert_eq!(out.len(), 8);
        let total: usize = out.iter().map(|(k, v)| k.len() + v.len()).sum();
        assert_eq!(total, MAX_TOTAL_BYTES);
        assert!(!out.contains_key("VAR_0008"));
    }

    #[test]
    fn byte_cap_stops_at_the_first_name_that_does_not_fit() {
        // Seven 32773-byte entries fit; the eighth would pass the cap, so it and the small name after it are dropped.
        let mut input: Vec<(String, String)> = (0..9)
            .map(|i| (format!("BIG_{i}"), "x".repeat(MAX_VALUE_BYTES)))
            .collect();
        input.push(("ZZZ".to_string(), "z".to_string()));
        let out = scrub(input);
        let names: Vec<&str> = out.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            vec![
                "BIG_0", "BIG_1", "BIG_2", "BIG_3", "BIG_4", "BIG_5", "BIG_6"
            ]
        );
    }
}
