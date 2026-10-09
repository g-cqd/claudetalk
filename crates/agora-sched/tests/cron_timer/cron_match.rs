//! Cron matching (steps, lists, ranges, names, dow 7, the Vixie day rule), calendar edges and civil-time conversions.

use super::{DAY_MS, cron, days_in_month, next_matches, utc};
use agora_sched::cron::{civil_from_ms, ms_from_civil};

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
