//! Timer instants (RFC 3339 and integer milliseconds), timer spec text, validation, first and next fires, and describe strings.

use super::{FIELD_COUNT_ERROR, MIN_MS, assert_not_a_time, every, spec, utc};
use agora_sched::timer::{
    TimerSpec, describe, first_fire, next_fire, parse_at, parse_spec, validate,
};

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
