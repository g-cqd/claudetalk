//! Unit tests for the cron parser, the next-fire search and the civil-time conversions.

use super::*;

fn at(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    ms_from_civil(year, month, day, hour, minute)
}

fn cron(expr: &str) -> Cron {
    Cron::parse(expr).expect("valid expression")
}

fn parse_error(expr: &str) -> String {
    Cron::parse(expr).expect_err("invalid expression")
}

fn bits(values: &[u32]) -> u64 {
    values.iter().fold(0, |mask, &v| mask | (1u64 << v))
}

#[test]
fn star_fields_cover_full_ranges() {
    let c = cron("* * * * *");
    assert_eq!(c.minutes, (1u64 << 60) - 1);
    assert_eq!(c.hours, (1u32 << 24) - 1);
    assert_eq!(c.dom, 0xFFFF_FFFE);
    assert_eq!(c.months, 0x1FFE);
    assert_eq!(c.dow, 0x7F);
    assert!(c.dom_any && c.dow_any);
}

#[test]
fn wrong_field_count_names_the_five_fields() {
    let msg = "a cron expression has 5 fields: minute hour day-of-month month day-of-week";
    assert_eq!(parse_error("* * * *"), msg);
    assert_eq!(parse_error("* * * * * *"), msg);
    assert_eq!(parse_error(""), msg);
}

#[test]
fn out_of_range_values_name_their_field() {
    assert_eq!(
        parse_error("61 * * * *"),
        "cron minute field: 61 is out of range 0-59"
    );
    assert_eq!(
        parse_error("0 24 * * *"),
        "cron hour field: 24 is out of range 0-23"
    );
    assert_eq!(
        parse_error("0 0 0 * *"),
        "cron day-of-month field: 0 is out of range 1-31"
    );
    assert_eq!(
        parse_error("0 0 1 13 *"),
        "cron month field: 13 is out of range 1-12"
    );
    assert_eq!(
        parse_error("0 0 * * 8"),
        "cron day-of-week field: 8 is out of range 0-7"
    );
    assert_eq!(
        parse_error("99999999999 * * * *"),
        "cron minute field: 99999999999 is out of range 0-59"
    );
}

#[test]
fn malformed_tokens_name_their_field() {
    assert_eq!(
        parse_error("jan * * * *"),
        "cron minute field: 'jan' is not a valid value"
    );
    assert_eq!(
        parse_error("0 0 * * sunday"),
        "cron day-of-week field: 'sunday' is not a valid value"
    );
    assert_eq!(
        parse_error("*/0 * * * *"),
        "cron minute field: step must be at least 1"
    );
    assert_eq!(
        parse_error("*/x * * * *"),
        "cron minute field: step 'x' is not a number"
    );
    assert!(parse_error("1,,2 * * * *").starts_with("cron minute field: empty list item"));
    assert_eq!(
        parse_error("30-10 * * * *"),
        "cron minute field: range 30-10 is backwards"
    );
}

#[test]
fn step_forms_expand_as_documented() {
    assert_eq!(cron("*/15 * * * *").minutes, bits(&[0, 15, 30, 45]));
    assert_eq!(cron("10-30/10 * * * *").minutes, bits(&[10, 20, 30]));
    // a/n is a-max/n: 5/20 runs 5, 25, 45.
    assert_eq!(cron("5/20 * * * *").minutes, bits(&[5, 25, 45]));
    assert_eq!(cron("1,2,5-7 * * * *").minutes, bits(&[1, 2, 5, 6, 7]));
}

#[test]
fn names_are_case_insensitive_and_work_in_ranges() {
    let weekdays = cron("0 9 * * mon-fri");
    assert_eq!(weekdays.hours, 1 << 9);
    assert_eq!(weekdays.dow, 0b0011_1110);
    assert!(weekdays.dom_any && !weekdays.dow_any);
    assert_eq!(cron("0 0 * JAN-MAR Mon-Fri"), cron("0 0 * jan-mar mon-fri"));
    assert_eq!(cron("0 0 1 jan *").months, 1 << 1);
}

#[test]
fn dow_seven_is_sunday() {
    assert_eq!(cron("0 0 * * 7").dow, 1);
    assert_eq!(cron("0 0 * * 7"), cron("0 0 * * 0"));
    assert_eq!(cron("0 0 * * 5-7").dow, 0b0110_0001);
}

#[test]
fn vixie_star_flags_follow_the_first_character() {
    let both = cron("0 0 13 * fri");
    assert!(!both.dom_any && !both.dow_any);
    let dom_only = cron("0 0 15 * *");
    assert!(!dom_only.dom_any && dom_only.dow_any);
    let dow_only = cron("0 0 * * fri");
    assert!(dow_only.dom_any && !dow_only.dow_any);
    // A stepped field starts with '*', so Vixie counts it as star too.
    let stepped = cron("0 0 */2 * */3");
    assert!(stepped.dom_any && stepped.dow_any);
}

#[test]
fn stepped_star_day_field_ands_with_the_weekday() {
    // "*/2" starts with '*', so the day rule is AND: odd days that are Mondays. 2025-01-06 is a Monday.
    // Under the OR rule Friday 2025-01-03 (an odd day) would fire first.
    let c = cron("0 0 */2 * mon");
    assert_eq!(
        c.next_after(at(2025, 1, 1, 0, 0)),
        Some(at(2025, 1, 13, 0, 0))
    );
    assert_eq!(
        c.next_after(at(2025, 1, 13, 0, 0)),
        Some(at(2025, 1, 27, 0, 0))
    );
    // Monday 2025-02-10 is an even day and is skipped; 2025-02-03 and 2025-02-17 are odd Mondays.
    assert_eq!(
        c.next_after(at(2025, 1, 27, 0, 0)),
        Some(at(2025, 2, 3, 0, 0))
    );
    assert_eq!(
        c.next_after(at(2025, 2, 3, 0, 0)),
        Some(at(2025, 2, 17, 0, 0))
    );
    assert_eq!(
        c.next_after(at(2025, 2, 17, 0, 0)),
        Some(at(2025, 3, 3, 0, 0))
    );
}

#[test]
fn step_expression_fires_every_quarter_hour() {
    let c = cron("*/15 * * * *");
    let base = at(2025, 1, 1, 0, 0);
    assert_eq!(c.next_after(base + 7 * MIN_MS), Some(at(2025, 1, 1, 0, 15)));
    assert_eq!(
        c.next_after(base + 15 * MIN_MS),
        Some(at(2025, 1, 1, 0, 30))
    );
    assert_eq!(
        c.next_after(at(2025, 1, 1, 0, 14) + MIN_MS - 1),
        Some(at(2025, 1, 1, 0, 15))
    );
    assert_eq!(
        c.next_after(at(2025, 1, 1, 0, 15) + 30_000),
        Some(at(2025, 1, 1, 0, 30))
    );
}

#[test]
fn weekday_range_skips_the_weekend() {
    // 2025-01-04 is a Saturday and 2025-01-06 a Monday.
    let c = cron("0 9 * * mon-fri");
    assert_eq!(
        c.next_after(at(2025, 1, 4, 10, 0)),
        Some(at(2025, 1, 6, 9, 0))
    );
}

#[test]
fn vixie_or_fires_on_the_thirteenth_and_on_fridays() {
    // 2025-01-03, 01-10 are Fridays; 2025-01-13 is a Monday.
    let c = cron("0 0 13 * fri");
    assert_eq!(
        c.next_after(at(2025, 1, 3, 0, 0)),
        Some(at(2025, 1, 10, 0, 0))
    );
    assert_eq!(
        c.next_after(at(2025, 1, 10, 0, 0)),
        Some(at(2025, 1, 13, 0, 0))
    );
}

#[test]
fn single_restricted_day_field_is_the_only_constraint() {
    assert_eq!(
        cron("0 0 15 * *").next_after(at(2025, 1, 1, 0, 0)),
        Some(at(2025, 1, 15, 0, 0))
    );
    assert_eq!(
        cron("0 0 * * mon").next_after(at(2025, 1, 1, 0, 0)),
        Some(at(2025, 1, 6, 0, 0))
    );
}

#[test]
fn feb_29_finds_the_next_leap_day() {
    assert_eq!(
        cron("0 0 29 2 *").next_after(at(2025, 3, 1, 0, 0)),
        Some(at(2028, 2, 29, 0, 0))
    );
}

#[test]
fn feb_29_finds_the_leap_day_past_a_skipped_century() {
    // 2100 is not a leap year, so the next Feb 29 after 2096 is 2104: 2920 days away, inside the 8-year horizon.
    assert_eq!(
        cron("0 0 29 2 *").next_after(at(2096, 3, 1, 0, 0)),
        Some(at(2104, 2, 29, 0, 0))
    );
}

#[test]
fn impossible_dates_give_none() {
    assert_eq!(cron("0 0 30 2 *").next_after(at(2025, 1, 1, 0, 0)), None);
    assert_eq!(cron("0 0 31 4 *").next_after(at(2025, 1, 1, 0, 0)), None);
}

#[test]
fn year_rollover_is_reached_from_the_last_minute() {
    let last = at(2025, 12, 31, 23, 59);
    assert_eq!(
        cron("* * * * *").next_after(last),
        Some(at(2026, 1, 1, 0, 0))
    );
    assert_eq!(
        cron("0 0 1 jan *").next_after(last),
        Some(at(2026, 1, 1, 0, 0))
    );
    assert_eq!(
        cron("59 23 31 12 *").next_after(at(2025, 1, 1, 0, 0)),
        Some(last)
    );
}

#[test]
fn next_after_is_strictly_after() {
    let c = cron("0 0 * * *");
    assert_eq!(
        c.next_after(at(2025, 1, 1, 0, 0)),
        Some(at(2025, 1, 2, 0, 0))
    );
    assert_eq!(
        c.next_after(at(2025, 1, 1, 0, 0) - 1),
        Some(at(2025, 1, 1, 0, 0))
    );
}

#[test]
fn next_after_handles_times_before_the_epoch() {
    assert_eq!(cron("0 0 * * *").next_after(-1), Some(0));
    assert_eq!(cron("* * * * *").next_after(i64::MAX), None);
}

#[test]
fn next_after_near_i64_min_finds_the_next_midnight_without_panicking() {
    let midnight = cron("0 0 * * *").next_after(i64::MIN);
    assert!(midnight.is_some_and(|t| t > i64::MIN));
    assert_eq!(midnight.map(|t| t.rem_euclid(DAY_MS)), Some(0));
}

#[test]
fn next_after_stops_at_the_end_of_representable_time() {
    // i64::MAX is 292278994-08-17T07:12:55.807Z. The first minute boundary after i64::MAX - 120 000 is 07:11:00.
    assert_eq!(
        cron("* * * * *").next_after(i64::MAX - 120_000),
        Some(9_223_372_036_854_660_000)
    );
    // The next midnight after 07:11 on the 17th lies past i64::MAX, so the search gives None rather than wrapping.
    assert_eq!(cron("0 0 * * *").next_after(i64::MAX - 120_000), None);
    // Midnight of the 17th is the last one at or before i64::MAX; nine days earlier is the first midnight after
    // i64::MAX - 10 days, and it is still representable.
    let last_midnight = i64::MAX / DAY_MS * DAY_MS;
    assert_eq!(
        cron("0 0 * * *").next_after(i64::MAX - 10 * DAY_MS),
        Some(last_midnight - 9 * DAY_MS)
    );
    // January 1st of the next year is beyond i64::MAX, so the month-start step overflows into None.
    assert_eq!(cron("0 0 1 1 *").next_after(i64::MAX - 10 * DAY_MS), None);
}

#[test]
fn civil_conversions_match_known_dates() {
    assert_eq!(civil_from_ms(0), (1970, 1, 1, 0, 0, 4));
    assert_eq!(civil_from_ms(-1), (1969, 12, 31, 23, 59, 3));
    assert_eq!(
        civil_from_ms(at(2025, 12, 31, 23, 59)),
        (2025, 12, 31, 23, 59, 3)
    );
}

#[test]
fn civil_round_trip_over_day_range() {
    for day in -1000..=20_000i64 {
        let ms = day * DAY_MS + 17 * HOUR_MS + 42 * MIN_MS;
        let (year, month, dom, hour, minute, weekday) = civil_from_ms(ms);
        assert!((1..=12).contains(&month), "month of day {day}");
        assert!((1..=31).contains(&dom), "day of month of day {day}");
        assert_eq!((hour, minute), (17, 42), "time of day of day {day}");
        assert_eq!(
            i64::from(weekday),
            (day + 4).rem_euclid(7),
            "weekday of day {day}"
        );
        assert_eq!(
            ms_from_civil(year, month, dom, hour, minute),
            ms,
            "round trip of day {day}"
        );
    }
}
