//! Cron parse errors: each field names itself, the field count is checked first, the first bad field wins.

use super::{FIELD_COUNT_ERROR, assert_parse_errors, parse_error};

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
