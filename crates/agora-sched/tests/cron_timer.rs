//! Cron expressions and timer specs of agora-sched through the public API only (SPEC 2.8, D11): field-level parse
//! errors, the step, list, range, name and day-of-week-7 forms, the Vixie day rule, leap days and month/year rollovers,
//! the strictly-after search, the civil-time conversions, RFC 3339 and interval parsing, validation, first and next
//! fire times (including coalescing after downtime) and the describe strings. Every instant is a UTC literal built with
//! `ms_from_civil`, so the file touches no clock, filesystem or network.

use agora_sched::cron::{Cron, civil_from_ms, ms_from_civil};
use agora_sched::timer::{
    TimerSpec, describe, first_fire, next_fire, parse_at, parse_spec, validate,
};

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

// ---------------------------------------------------------------------------------------------------------------
// Cron parse errors: each field names itself, the field count is checked first, the first bad field wins.
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn wrong_field_count_reports_the_five_field_layout() {
    for expr in ["", "*/15", "0 9 * *", "0 9 * * * *", "@daily"] {
        assert_eq!(parse_error(expr), FIELD_COUNT_ERROR, "expression '{expr}'");
    }
}

#[test]
fn minute_errors_name_the_minute_field() {
    assert_parse_errors(&[
        ("60 * * * *", "cron minute field: 60 is out of range 0-59"),
        ("*/0 * * * *", "cron minute field: step must be at least 1"),
        ("*/x * * * *", "cron minute field: step 'x' is not a number"),
        ("5-1 * * * *", "cron minute field: range 5-1 is backwards"),
        (
            "1,,2 * * * *",
            "cron minute field: empty list item in '1,,2'",
        ),
        ("1x * * * *", "cron minute field: '1x' is not a valid value"),
        ("1-x * * * *", "cron minute field: 'x' is not a valid value"),
    ]);
}

#[test]
fn hour_errors_name_the_hour_field() {
    assert_parse_errors(&[
        ("0 24 * * *", "cron hour field: 24 is out of range 0-23"),
        ("0 9-25 * * *", "cron hour field: 25 is out of range 0-23"),
        ("0 */0 * * *", "cron hour field: step must be at least 1"),
        (
            "0 noon * * *",
            "cron hour field: 'noon' is not a valid value",
        ),
    ]);
}

#[test]
fn day_of_month_errors_name_the_day_of_month_field() {
    assert_parse_errors(&[
        (
            "0 0 0 * *",
            "cron day-of-month field: 0 is out of range 1-31",
        ),
        (
            "0 0 32 * *",
            "cron day-of-month field: 32 is out of range 1-31",
        ),
        (
            "0 0 1-40 * *",
            "cron day-of-month field: 40 is out of range 1-31",
        ),
        (
            "0 0 jan * *",
            "cron day-of-month field: 'jan' is not a valid value",
        ),
    ]);
}

#[test]
fn month_errors_name_the_month_field() {
    assert_parse_errors(&[
        ("0 0 1 13 *", "cron month field: 13 is out of range 1-12"),
        ("0 0 1 0 *", "cron month field: 0 is out of range 1-12"),
        (
            "0 0 1 jan-13 *",
            "cron month field: 13 is out of range 1-12",
        ),
        (
            "0 0 1 mon *",
            "cron month field: 'mon' is not a valid value",
        ),
    ]);
}

#[test]
fn day_of_week_errors_name_the_day_of_week_field() {
    assert_parse_errors(&[
        ("0 0 * * 8", "cron day-of-week field: 8 is out of range 0-7"),
        (
            "0 0 * * funday",
            "cron day-of-week field: 'funday' is not a valid value",
        ),
        (
            "0 0 * * 5-2",
            "cron day-of-week field: range 5-2 is backwards",
        ),
    ]);
}

#[test]
fn first_bad_field_is_the_one_reported() {
    assert_eq!(
        parse_error("60 99 0 13 8"),
        "cron minute field: 60 is out of range 0-59"
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Cron matching: steps, lists, ranges, names, dow 7, and the Vixie day rule.
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn star_slash_fifteen_fires_on_each_quarter_hour() {
    assert_eq!(
        next_matches("*/15 * * * *", utc(2026, 10, 8, 9, 7), 4),
        [
            utc(2026, 10, 8, 9, 15),
            utc(2026, 10, 8, 9, 30),
            utc(2026, 10, 8, 9, 45),
            utc(2026, 10, 8, 10, 0),
        ]
    );
}

#[test]
fn comma_lists_and_hour_ranges_combine_within_the_day() {
    // Thursday 2026-10-08 and Friday 2026-10-09 are both weekdays.
    assert_eq!(
        next_matches("0,30 9-17 * * 1-5", utc(2026, 10, 8, 17, 0), 3),
        [
            utc(2026, 10, 8, 17, 30),
            utc(2026, 10, 9, 9, 0),
            utc(2026, 10, 9, 9, 30),
        ]
    );
}

#[test]
fn weekday_range_skips_saturday_and_sunday() {
    // Friday 17:30 is the last slot of the window; the next one is Monday 09:00.
    assert_eq!(
        cron("0,30 9-17 * * 1-5").next_after(utc(2026, 10, 9, 17, 30)),
        Some(utc(2026, 10, 12, 9, 0))
    );
}

#[test]
fn month_and_weekday_names_select_the_named_days() {
    // "jan,jul" with "mon" and a wildcard day of month: Mondays in January and July, at 09:00.
    assert_eq!(
        next_matches("0 9 * jan,jul mon", utc(2026, 10, 8, 10, 0), 3),
        [
            utc(2027, 1, 4, 9, 0),
            utc(2027, 1, 11, 9, 0),
            utc(2027, 1, 18, 9, 0),
        ]
    );
    assert_eq!(
        cron("0 9 * jan,jul mon").next_after(utc(2027, 1, 25, 10, 0)),
        Some(utc(2027, 7, 5, 9, 0))
    );
}

#[test]
fn names_are_case_insensitive() {
    let from = utc(2026, 10, 8, 10, 0);
    assert_eq!(
        next_matches("0 9 * JAN,Jul Mon", from, 3),
        next_matches("0 9 * jan,jul mon", from, 3)
    );
}

#[test]
fn day_of_week_zero_seven_and_sun_all_mean_sunday() {
    // Saturday 2026-10-10 10:00: the next two Sundays at 09:00.
    let from = utc(2026, 10, 10, 10, 0);
    let sundays = [utc(2026, 10, 11, 9, 0), utc(2026, 10, 18, 9, 0)];
    for expr in ["0 9 * * 0", "0 9 * * 7", "0 9 * * sun"] {
        assert_eq!(next_matches(expr, from, 2), sundays, "expression '{expr}'");
    }
}

#[test]
fn day_of_week_range_ending_at_seven_includes_sunday() {
    // "5-7" is Friday through Sunday.
    assert_eq!(
        next_matches("0 9 * * 5-7", utc(2026, 10, 8, 10, 0), 3),
        [
            utc(2026, 10, 9, 9, 0),
            utc(2026, 10, 10, 9, 0),
            utc(2026, 10, 11, 9, 0),
        ]
    );
}

#[test]
fn both_day_fields_restricted_fire_on_either_match() {
    // The 13th (Tuesday 2026-10-13) or any Friday (2026-10-09, 2026-10-16).
    assert_eq!(
        next_matches("0 9 13 * fri", utc(2026, 10, 8, 10, 0), 3),
        [
            utc(2026, 10, 9, 9, 0),
            utc(2026, 10, 13, 9, 0),
            utc(2026, 10, 16, 9, 0),
        ]
    );
}

#[test]
fn a_single_restricted_day_field_is_the_only_constraint() {
    // Day 13 only: Friday 2026-10-09 does not count.
    assert_eq!(
        next_matches("0 9 13 * *", utc(2026, 10, 8, 10, 0), 1),
        [utc(2026, 10, 13, 9, 0)]
    );
    // Friday only: the 13th does not count.
    assert_eq!(
        next_matches("0 9 * * fri", utc(2026, 10, 8, 10, 0), 2),
        [utc(2026, 10, 9, 9, 0), utc(2026, 10, 16, 9, 0)]
    );
}

#[test]
fn an_impossible_day_of_month_still_fires_on_the_weekday_under_the_or_rule() {
    // Day 30 never falls in February, but the Vixie OR lets any Monday in February match.
    assert_eq!(
        cron("0 0 30 2 mon").next_after(utc(2026, 10, 8, 0, 0)),
        Some(utc(2027, 2, 1, 0, 0))
    );
}

#[test]
fn impossible_dates_never_fire() {
    for expr in ["0 0 30 2 *", "0 0 31 2 *", "0 0 31 4,6,9,11 *"] {
        assert_eq!(
            cron(expr).next_after(utc(2026, 10, 8, 0, 0)),
            None,
            "expression '{expr}'"
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Cron calendar edges: leap days, rollovers, and the strictly-after search.
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn feb_29_fires_on_the_leap_day_of_the_next_leap_year() {
    assert_eq!(
        cron("0 0 29 2 *").next_after(utc(2025, 3, 1, 0, 0)),
        Some(utc(2028, 2, 29, 0, 0))
    );
}

#[test]
fn feb_29_cadence_skips_the_three_common_years() {
    // From the 2028 leap day, 2029 to 2031 have no Feb 29; 2032 does.
    assert_eq!(
        cron("0 0 29 2 *").next_after(utc(2028, 2, 29, 0, 0)),
        Some(utc(2032, 2, 29, 0, 0))
    );
}

#[test]
fn feb_29_skips_century_years_that_are_not_leap() {
    // 2100 is not a leap year, so from 2099 the next leap day is 2104 (1825 days away, inside the search horizon).
    assert_eq!(
        cron("0 0 29 2 *").next_after(utc(2099, 3, 1, 0, 0)),
        Some(utc(2104, 2, 29, 0, 0))
    );
}

#[test]
fn monthly_fire_rolls_over_month_and_year_boundaries() {
    assert_eq!(
        next_matches("0 0 1 * *", utc(2026, 10, 31, 12, 0), 3),
        [
            utc(2026, 11, 1, 0, 0),
            utc(2026, 12, 1, 0, 0),
            utc(2027, 1, 1, 0, 0),
        ]
    );
}

#[test]
fn daily_fire_reaches_february_29_only_in_leap_years() {
    assert_eq!(
        next_matches("0 0 * * *", utc(2028, 2, 28, 23, 59), 2),
        [utc(2028, 2, 29, 0, 0), utc(2028, 3, 1, 0, 0)]
    );
    assert_eq!(
        next_matches("0 0 * * *", utc(2027, 2, 28, 23, 59), 1),
        [utc(2027, 3, 1, 0, 0)]
    );
}

#[test]
fn next_after_is_strictly_after_the_instant() {
    let daily_nine = cron("0 9 * * *");
    let nine = utc(2026, 10, 8, 9, 0);
    // One millisecond before the boundary, the boundary itself is still ahead.
    assert_eq!(daily_nine.next_after(nine - 1), Some(nine));
    // Exactly on the boundary, that match is consumed.
    assert_eq!(daily_nine.next_after(nine), Some(utc(2026, 10, 9, 9, 0)));
    // One millisecond after the boundary, the boundary is behind us.
    assert_eq!(
        daily_nine.next_after(nine + 1),
        Some(utc(2026, 10, 9, 9, 0))
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Civil-time conversions.
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn civil_round_trip_holds_for_ten_thousand_consecutive_days() {
    for day in -2_000..8_000_i64 {
        let ms = day * DAY_MS;
        let (year, month, dom, hour, minute, weekday) = civil_from_ms(ms);
        assert_eq!((hour, minute), (0, 0), "day {day}");
        assert_eq!(ms_from_civil(year, month, dom, 0, 0), ms, "day {day}");
        assert!((1..=days_in_month(year, month)).contains(&dom), "day {day}");

        // Independent oracle: the following day is the next date on the calendar, and the weekday advances by one.
        let (ny, nm, nd, _, _, next_weekday) = civil_from_ms(ms + DAY_MS);
        let expected_next = if dom < days_in_month(year, month) {
            (year, month, dom + 1)
        } else if month < 12 {
            (year, month + 1, 1)
        } else {
            (year + 1, 1, 1)
        };
        assert_eq!((ny, nm, nd), expected_next, "day {day}");
        assert_eq!(next_weekday, (weekday + 1) % 7, "day {day}");
    }
}

#[test]
fn weekdays_of_known_dates_are_correct() {
    let weekday = |ms: i64| civil_from_ms(ms).5;
    assert_eq!(
        weekday(utc(1970, 1, 1, 0, 0)),
        4,
        "1970-01-01 is a Thursday"
    );
    assert_eq!(
        weekday(utc(2000, 1, 1, 0, 0)),
        6,
        "2000-01-01 is a Saturday"
    );
    assert_eq!(
        weekday(utc(2026, 10, 8, 9, 30)),
        4,
        "2026-10-08 is a Thursday"
    );
    assert_eq!(
        weekday(utc(2000, 2, 29, 0, 0)),
        2,
        "2000-02-29 is a Tuesday"
    );
}

#[test]
fn civil_from_ms_of_the_epoch_and_the_instant_before_it() {
    assert_eq!(utc(1970, 1, 1, 0, 0), 0);
    assert_eq!(civil_from_ms(0), (1970, 1, 1, 0, 0, 4));
    assert_eq!(civil_from_ms(-1), (1969, 12, 31, 23, 59, 3));
}

#[test]
fn civil_from_ms_drops_seconds_and_milliseconds() {
    assert_eq!(
        civil_from_ms(utc(2026, 10, 8, 9, 30) + 59_999),
        (2026, 10, 8, 9, 30, 4)
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Timer instants: RFC 3339 and integer milliseconds.
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn parse_at_zulu_suffix_is_utc_in_either_case() {
    let instant = utc(2026, 10, 8, 9, 30);
    assert_eq!(parse_at("2026-10-08T09:30:00Z"), Ok(instant));
    assert_eq!(parse_at("2026-10-08t09:30:00z"), Ok(instant));
    assert_eq!(
        parse_at("2026-10-08T09:30Z"),
        Ok(instant),
        "seconds are optional"
    );
}

#[test]
fn parse_at_numeric_offsets_convert_to_utc_across_midnight() {
    assert_eq!(
        parse_at("2026-10-08T09:30:00+02:00"),
        Ok(utc(2026, 10, 8, 7, 30))
    );
    assert_eq!(
        parse_at("2026-10-08T09:30:00-05:30"),
        Ok(utc(2026, 10, 8, 15, 0))
    );
    assert_eq!(
        parse_at("2026-10-08T00:30:00+01:00"),
        Ok(utc(2026, 10, 7, 23, 30))
    );
    assert_eq!(
        parse_at("2026-10-08T23:30:00-02:00"),
        Ok(utc(2026, 10, 9, 1, 30))
    );
}

#[test]
fn parse_at_keeps_milliseconds_and_truncates_finer_digits() {
    let base = utc(2026, 10, 8, 9, 30);
    assert_eq!(parse_at("2026-10-08T09:30:00.250Z"), Ok(base + 250));
    assert_eq!(parse_at("2026-10-08T09:30:00.5Z"), Ok(base + 500));
    assert_eq!(parse_at("2026-10-08T09:30:00.123456Z"), Ok(base + 123));
}

#[test]
fn parse_at_accepts_a_space_in_place_of_the_t() {
    assert_eq!(
        parse_at("2026-10-08 09:30:00Z"),
        Ok(utc(2026, 10, 8, 9, 30))
    );
    assert_eq!(
        parse_at("2026-10-08 09:30:00+02:00"),
        Ok(utc(2026, 10, 8, 7, 30))
    );
}

#[test]
fn parse_at_plain_integer_is_milliseconds_since_the_epoch() {
    assert_eq!(parse_at("1700000000000"), Ok(1_700_000_000_000));
    assert_eq!(parse_at("0"), Ok(0));
    assert_eq!(parse_at("-1500"), Ok(-1_500));
}

#[test]
fn parse_at_rejects_impossible_calendar_dates() {
    for text in [
        "2025-02-29T00:00Z",
        "2026-04-31T00:00Z",
        "2026-13-01T00:00Z",
        "2026-10-00T00:00Z",
    ] {
        assert_not_a_time(text);
    }
}

#[test]
fn parse_at_accepts_feb_29_only_in_leap_years() {
    assert_eq!(parse_at("2028-02-29T00:00Z"), Ok(utc(2028, 2, 29, 0, 0)));
    assert_eq!(parse_at("2000-02-29T00:00Z"), Ok(utc(2000, 2, 29, 0, 0)));
    // 2100 is divisible by 100 and not by 400.
    assert_not_a_time("2100-02-29T00:00Z");
}

#[test]
fn parse_at_rejects_out_of_range_clock_fields() {
    for text in [
        "2026-10-08T24:00Z",
        "2026-10-08T09:60Z",
        "2026-10-08T09:30:61Z",
    ] {
        assert_not_a_time(text);
    }
}

#[test]
fn parse_at_requires_a_zone_and_a_well_formed_offset() {
    for text in [
        "2026-10-08T09:30:00",
        "2026-10-08T09:30:00+0200",
        "2026-10-08T09:30:00+02:60",
        "2026-10-08T09:30:00+24:00",
        "2026-10-08T09:30:00Zjunk",
    ] {
        assert_not_a_time(text);
    }
}

#[test]
fn parse_at_rejects_text_that_is_not_a_time() {
    for text in ["", "-", "yesterday", "2026-10-08", "99999999999999999999"] {
        assert_not_a_time(text);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Timer spec text: at:, every:, cron: (syntax only; validate checks the rest).
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn parse_spec_reads_at_as_rfc3339_or_integer_milliseconds() {
    assert_eq!(
        parse_spec("at:2026-10-08T09:30:00Z"),
        Ok(TimerSpec::At {
            at_ms: utc(2026, 10, 8, 9, 30)
        })
    );
    assert_eq!(parse_spec("at:5000"), Ok(TimerSpec::At { at_ms: 5_000 }));
}

#[test]
fn parse_spec_reads_every_with_an_optional_unit_suffix_into_seconds() {
    assert_eq!(parse_spec("every:90"), Ok(every(90, None)));
    assert_eq!(parse_spec("every:30s"), Ok(every(30, None)));
    assert_eq!(parse_spec("every:5m"), Ok(every(300, None)));
    assert_eq!(parse_spec("every:2h"), Ok(every(7_200, None)));
    assert_eq!(parse_spec("every:1d"), Ok(every(86_400, None)));
}

#[test]
fn parse_spec_keeps_cron_text_verbatim() {
    assert_eq!(
        parse_spec("cron:0 9 * * mon-fri"),
        Ok(TimerSpec::Cron {
            expr: "0 9 * * mon-fri".to_string()
        })
    );
}

#[test]
fn parse_spec_rejects_unknown_kinds_and_malformed_values() {
    for text in ["", "weekly:1", "AT:5000", "0 9 * * *"] {
        let message = parse_spec(text).expect_err("unknown schedule kind");
        assert!(message.starts_with("a schedule is"), "'{text}': {message}");
    }
    assert!(
        parse_spec("at:tomorrow")
            .expect_err("not a time")
            .starts_with("'tomorrow' is not a time")
    );
    for interval in ["", "h", "-5", "1.5h", "5ms"] {
        let text = format!("every:{interval}");
        let message = parse_spec(&text).expect_err("malformed interval");
        assert!(
            message.starts_with(&format!("'{interval}' is not an interval")),
            "'{text}': {message}"
        );
    }
    // u64::MAX days overflows the seconds count: an error, not a wrapped value.
    assert!(
        parse_spec("every:18446744073709551615d")
            .expect_err("overflowing interval")
            .starts_with("'18446744073709551615d' is too large")
    );
}

// ---------------------------------------------------------------------------------------------------------------
// Validation: the minimum interval and cron syntax.
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn validate_rejects_every_below_sixty_seconds() {
    let too_short = Err("every must be at least 60 seconds".to_string());
    for text in ["every:59s", "every:0", "every:59"] {
        assert_eq!(validate(&spec(text)), too_short, "'{text}'");
    }
    for text in ["every:60", "every:1m", "every:1h"] {
        assert_eq!(validate(&spec(text)), Ok(()), "'{text}'");
    }
}

#[test]
fn validate_checks_cron_syntax_and_names_the_bad_field() {
    assert_eq!(validate(&spec("cron:*/15 * * * *")), Ok(()));
    assert_eq!(
        validate(&spec("cron:0 9 * *")),
        Err(FIELD_COUNT_ERROR.to_string())
    );
    assert_eq!(
        validate(&spec("cron:61 * * * *")),
        Err("cron minute field: 61 is out of range 0-59".to_string())
    );
}

// ---------------------------------------------------------------------------------------------------------------
// First fire: the instant after creation for each kind, and no overflow at the timeline's end.
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn first_fire_of_an_at_timer_is_its_instant_even_when_already_past() {
    let created = utc(2026, 10, 8, 10, 0);
    let past = TimerSpec::At {
        at_ms: utc(2026, 10, 8, 9, 0),
    };
    let future = TimerSpec::At {
        at_ms: utc(2026, 10, 8, 12, 0),
    };
    assert_eq!(first_fire(&past, created), Some(utc(2026, 10, 8, 9, 0)));
    assert_eq!(first_fire(&future, created), Some(utc(2026, 10, 8, 12, 0)));
}

#[test]
fn first_fire_of_an_every_timer_is_one_period_after_creation_or_its_anchor() {
    let created = utc(2026, 10, 8, 10, 0);
    assert_eq!(
        first_fire(&every(300, None), created),
        Some(created + 5 * MIN_MS)
    );
    assert_eq!(
        first_fire(&every(300, Some(utc(2026, 10, 8, 9, 0))), created),
        Some(utc(2026, 10, 8, 9, 0))
    );
}

#[test]
fn first_fire_of_a_cron_timer_is_the_next_match_after_creation() {
    let created = utc(2026, 10, 8, 10, 0);
    assert_eq!(
        first_fire(&spec("cron:0 9 * * *"), created),
        Some(utc(2026, 10, 9, 9, 0))
    );
    assert_eq!(first_fire(&spec("cron:99 * * * *"), created), None);
}

#[test]
fn first_fire_returns_none_instead_of_overflowing_the_timeline() {
    // 9_223_372_036_854_775 seconds is the largest period whose millisecond value fits in i64.
    let largest_period_s = 9_223_372_036_854_775;
    assert_eq!(
        first_fire(&every(largest_period_s, None), 1),
        Some(9_223_372_036_854_775_001)
    );
    assert_eq!(first_fire(&every(largest_period_s, None), 1_000), None);
    assert_eq!(first_fire(&every(largest_period_s + 1, None), 0), None);
}

// ---------------------------------------------------------------------------------------------------------------
// Next fire after a fire and a downtime: missed fires coalesce into one, strictly after now.
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn at_timer_has_no_next_fire_after_it_fired() {
    let at = TimerSpec::At {
        at_ms: utc(2026, 10, 8, 9, 0),
    };
    assert_eq!(
        next_fire(&at, utc(2026, 10, 8, 9, 0), utc(2026, 10, 8, 9, 0)),
        None
    );
    assert_eq!(
        next_fire(&at, utc(2026, 10, 8, 9, 0), utc(2026, 10, 9, 9, 0)),
        None
    );
}

#[test]
fn every_timer_coalesces_three_hours_of_downtime_into_one_fire() {
    // Fired at 09:00 every 5 minutes; the daemon is back at 12:02 after three hours down.
    let fired = utc(2026, 10, 8, 9, 0);
    let now = utc(2026, 10, 8, 12, 2);
    assert_eq!(
        next_fire(&every(300, None), fired, now),
        Some(utc(2026, 10, 8, 12, 5))
    );
}

#[test]
fn every_timer_on_a_grid_point_fires_one_period_later() {
    let fired = utc(2026, 10, 8, 9, 0);
    assert_eq!(
        next_fire(&every(300, None), fired, utc(2026, 10, 8, 12, 0)),
        Some(utc(2026, 10, 8, 12, 5))
    );
}

#[test]
fn every_timer_with_an_explicit_start_keeps_that_grid_not_the_fire_time() {
    // The start is 08:57:30, so the grid is 08:57:30 + k * 5 minutes; the fire at 09:00 is off that grid.
    let start = utc(2026, 10, 8, 8, 57) + 30_000;
    let anchored = TimerSpec::Every {
        every_s: 300,
        start_ms: Some(start),
    };
    assert_eq!(
        next_fire(&anchored, utc(2026, 10, 8, 9, 0), utc(2026, 10, 8, 12, 2)),
        Some(utc(2026, 10, 8, 12, 2) + 30_000)
    );
}

#[test]
fn cron_timer_after_downtime_fires_at_the_next_match_after_now() {
    // Fired Thursday 09:00; back on Saturday 15:00 after the missed Friday and Saturday fires.
    assert_eq!(
        next_fire(
            &spec("cron:0 9 * * *"),
            utc(2026, 10, 8, 9, 0),
            utc(2026, 10, 10, 15, 0)
        ),
        Some(utc(2026, 10, 11, 9, 0))
    );
}

#[test]
fn zero_period_every_timer_has_no_next_fire() {
    assert_eq!(next_fire(&every(0, None), 0, 10), None);
}

// ---------------------------------------------------------------------------------------------------------------
// Describe strings.
// ---------------------------------------------------------------------------------------------------------------

#[test]
fn describe_at_shows_the_utc_instant_with_milliseconds_only_when_present() {
    assert_eq!(
        describe(&TimerSpec::At {
            at_ms: utc(2026, 10, 8, 9, 30)
        }),
        "at 2026-10-08T09:30:00Z"
    );
    assert_eq!(
        describe(&TimerSpec::At {
            at_ms: utc(2026, 10, 8, 9, 30) + 250
        }),
        "at 2026-10-08T09:30:00.250Z"
    );
    assert_eq!(
        describe(&TimerSpec::At { at_ms: -1_000 }),
        "at 1969-12-31T23:59:59Z"
    );
}

#[test]
fn describe_every_uses_the_largest_unit_that_divides_the_interval() {
    assert_eq!(describe(&every(90, None)), "every 90 s");
    assert_eq!(describe(&every(120, None)), "every 2 min");
    assert_eq!(describe(&every(300, None)), "every 5 min");
    assert_eq!(describe(&every(7_200, None)), "every 2 h");
    assert_eq!(describe(&every(90_000, None)), "every 25 h");
    assert_eq!(describe(&every(86_400, None)), "every 1 d");
}

#[test]
fn describe_cron_names_the_expression_and_utc() {
    assert_eq!(
        describe(&spec("cron:0 9 * * mon-fri")),
        "cron 0 9 * * mon-fri (UTC)"
    );
}

#[test]
fn describe_at_text_parses_back_to_the_same_instant() {
    for at_ms in [
        utc(2026, 10, 8, 9, 30),
        utc(2026, 10, 8, 9, 30) + 250,
        -1_000,
        0,
    ] {
        let text = describe(&TimerSpec::At { at_ms });
        let time = text
            .strip_prefix("at ")
            .expect("describe starts with 'at '");
        assert_eq!(parse_at(time), Ok(at_ms), "{text}");
    }
}
