//! Cron expressions and timer specs of agora-sched through the public API only (SPEC 2.8, D11): field-level parse
//! errors, the step, list, range, name and day-of-week-7 forms, the Vixie day rule, leap days and month/year rollovers,
//! the strictly-after search, the civil-time conversions, RFC 3339 and interval parsing, validation, first and next
//! fire times (including coalescing after downtime) and the describe strings. Every instant is a UTC literal built with
//! `ms_from_civil`, so the file touches no clock, filesystem or network.

mod cron_match;
mod cron_parse;
mod timer_spec;

use agora_sched::cron::{Cron, ms_from_civil};
use agora_sched::timer::{TimerSpec, parse_at, parse_spec};

const MIN_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;
const FIELD_COUNT_ERROR: &str =
    "a cron expression has 5 fields: minute hour day-of-month month day-of-week";

fn utc(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    ms_from_civil(year, month, day, hour, minute)
}

fn cron(expr: &str) -> Cron {
    Cron::parse(expr).expect("expression parses")
}

/// The next `count` matches after `start_ms`, each search starting from the previous match.
fn next_matches(expr: &str, start_ms: i64, count: usize) -> Vec<i64> {
    let cron = cron(expr);
    let mut matches = Vec::with_capacity(count);
    let mut after = start_ms;
    for _ in 0..count {
        let Some(next) = cron.next_after(after) else {
            break;
        };
        matches.push(next);
        after = next;
    }
    matches
}

fn parse_error(expr: &str) -> String {
    match Cron::parse(expr) {
        Ok(parsed) => panic!("'{expr}' should be rejected, parsed as {parsed:?}"),
        Err(message) => message,
    }
}

fn assert_parse_errors(cases: &[(&str, &str)]) {
    for (expr, message) in cases {
        assert_eq!(parse_error(expr), *message, "expression '{expr}'");
    }
}

fn assert_not_a_time(text: &str) {
    match parse_at(text) {
        Ok(ms) => panic!("'{text}' should be rejected, parsed as {ms}"),
        Err(message) => {
            let prefix = format!("'{text}' is not a time");
            assert!(message.starts_with(&prefix), "{message}");
        }
    }
}

fn spec(text: &str) -> TimerSpec {
    parse_spec(text).expect("spec parses")
}

fn every(every_s: u64, start_ms: Option<i64>) -> TimerSpec {
    TimerSpec::Every { every_s, start_ms }
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// An independent calendar oracle for the round-trip test (the library computes its own).
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}
