//! Persisted timers: `at`, `every`, `cron` (D11).

use crate::cron::{Cron, civil_from_ms};

mod rfc3339;

use rfc3339::parse_rfc3339;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimerSpec {
    At {
        at_ms: i64,
    },
    Every {
        every_s: u64,
        #[serde(default)]
        start_ms: Option<i64>,
    },
    Cron {
        expr: String,
    },
}

pub const MIN_EVERY_S: u64 = 60;

const TIME_HINT: &str = "use milliseconds since the epoch or RFC 3339 like 2026-10-08T09:30:00Z";
const SCHEDULE_HINT: &str = "a schedule is at:<time>, every:<interval> or cron:<5 fields>";

/// integer ms since epoch, or RFC 3339 "YYYY-MM-DDTHH:MM[:SS[.fff]](Z|+-HH:MM)" -> UTC ms.
pub fn parse_at(text: &str) -> Result<i64, String> {
    let not_a_time = || format!("'{text}' is not a time: {TIME_HINT}");
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    if all_digits(unsigned) {
        return text.parse::<i64>().map_err(|_| not_a_time());
    }
    parse_rfc3339(text).ok_or_else(not_a_time)
}

/// "at:<time>" | "every:<secs>[s|m|h|d]" | "cron:<5 fields>" -> spec (CLI/MCP helper).
/// Syntax only: the interval minimum and the cron fields are checked by `validate`.
pub fn parse_spec(text: &str) -> Result<TimerSpec, String> {
    if let Some(time) = text.strip_prefix("at:") {
        return Ok(TimerSpec::At {
            at_ms: parse_at(time)?,
        });
    }
    if let Some(interval) = text.strip_prefix("every:") {
        return Ok(TimerSpec::Every {
            every_s: parse_interval(interval)?,
            start_ms: None,
        });
    }
    if let Some(expr) = text.strip_prefix("cron:") {
        return Ok(TimerSpec::Cron {
            expr: expr.to_string(),
        });
    }
    Err(SCHEDULE_HINT.to_string())
}

pub fn validate(spec: &TimerSpec) -> Result<(), String> {
    match spec {
        TimerSpec::At { .. } => Ok(()),
        TimerSpec::Every { every_s, .. } if *every_s < MIN_EVERY_S => {
            Err(format!("every must be at least {MIN_EVERY_S} seconds"))
        }
        TimerSpec::Every { every_s, .. } => period_ms(*every_s)
            .map(|_| ())
            .ok_or_else(|| "every is too large".to_string()),
        TimerSpec::Cron { expr } => Cron::parse(expr).map(|_| ()),
    }
}

/// First fire at/after creation: At -> at_ms (fires at once if already past); Every -> start_ms.unwrap_or(created+every);
/// Cron -> next_after(created).
pub fn first_fire(spec: &TimerSpec, created_ms: i64) -> Option<i64> {
    match spec {
        TimerSpec::At { at_ms } => Some(*at_ms),
        TimerSpec::Every { every_s, start_ms } => match start_ms {
            Some(start) => Some(*start),
            None => created_ms.checked_add(period_ms(*every_s)?),
        },
        TimerSpec::Cron { expr } => Cron::parse(expr).ok()?.next_after(created_ms),
    }
}

/// After a fire at `fired_ms` while `now_ms` (>= fired_ms): the next fire strictly after now (missed fires coalesce).
/// At -> None. Every -> smallest start + k*every > now. Cron -> next_after(now).
/// `fired_ms` must be the SCHEDULED fire time (the timer's `next_fire_ms`), not the wall-clock time the fire happened:
/// an Every grid without `start_ms` is anchored on it, so a late fire does not drift the schedule.
pub fn next_fire(spec: &TimerSpec, fired_ms: i64, now_ms: i64) -> Option<i64> {
    match spec {
        TimerSpec::At { .. } => None,
        TimerSpec::Every { every_s, start_ms } => {
            // A zero period would divide by zero; it also never fires.
            let period = period_ms(*every_s).filter(|period| *period > 0)?;
            let anchor = start_ms.unwrap_or(fired_ms);
            // The grid is anchor + k*period; k = floor((now - anchor) / period) + 1 is the first point after now.
            let steps = now_ms
                .checked_sub(anchor)?
                .div_euclid(period)
                .checked_add(1)?;
            anchor.checked_add(steps.checked_mul(period)?)
        }
        TimerSpec::Cron { expr } => Cron::parse(expr).ok()?.next_after(now_ms.max(fired_ms)),
    }
}

pub fn describe(spec: &TimerSpec) -> String {
    match spec {
        TimerSpec::At { at_ms } => format!("at {}", format_utc(*at_ms)),
        TimerSpec::Every { every_s, .. } => format!("every {}", describe_interval(*every_s)),
        TimerSpec::Cron { expr } => format!("cron {expr} (UTC)"),
    }
}

fn all_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

fn period_ms(every_s: u64) -> Option<i64> {
    i64::try_from(every_s).ok()?.checked_mul(1_000)
}

/// The number of an `every:` value, with an optional unit suffix (no unit = seconds), as whole seconds.
fn parse_interval(text: &str) -> Result<u64, String> {
    let (number, unit_secs) = [("s", 1), ("m", 60), ("h", 3_600), ("d", 86_400)]
        .into_iter()
        .find_map(|(suffix, secs)| text.strip_suffix(suffix).map(|number| (number, secs)))
        .unwrap_or((text, 1));
    if !all_digits(number) {
        return Err(format!(
            "'{text}' is not an interval: use whole seconds, or a whole number with s, m, h or d, like every:5m"
        ));
    }
    let too_large = || format!("'{text}' is too large");
    let count: u64 = number.parse().map_err(|_| too_large())?;
    count.checked_mul(unit_secs).ok_or_else(too_large)
}

/// The largest unit that divides the interval exactly, so the description reads as the operator wrote it.
fn describe_interval(every_s: u64) -> String {
    for (secs, unit) in [(86_400, "d"), (3_600, "h"), (60, "min")] {
        if every_s > 0 && every_s.is_multiple_of(secs) {
            return format!("{} {unit}", every_s / secs);
        }
    }
    format!("{every_s} s")
}

/// "YYYY-MM-DDTHH:MM:SS[.mmm]Z" for a UTC instant; the fraction appears only when it is non-zero.
fn format_utc(ms: i64) -> String {
    let (year, month, day, hour, minute, _) = civil_from_ms(ms);
    let second = ms.div_euclid(1_000).rem_euclid(60);
    let millis = ms.rem_euclid(1_000);
    let fraction = if millis == 0 {
        String::new()
    } else {
        format!(".{millis:03}")
    };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}{fraction}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::ms_from_civil;

    fn ms(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        ms_from_civil(year, month, day, hour, minute)
    }

    fn every(every_s: u64) -> TimerSpec {
        TimerSpec::Every {
            every_s,
            start_ms: None,
        }
    }

    fn not_a_time(text: &str) -> String {
        format!("'{text}' is not a time: {TIME_HINT}")
    }

    #[test]
    fn parse_at_takes_integer_milliseconds() {
        assert_eq!(parse_at("1500"), Ok(1_500));
        assert_eq!(parse_at("0"), Ok(0));
        assert_eq!(parse_at("-1500"), Ok(-1_500));
    }

    #[test]
    fn parse_at_zulu_is_utc_in_either_case() {
        let base = ms(2026, 10, 8, 9, 30);
        assert_eq!(parse_at("2026-10-08T09:30:00Z"), Ok(base));
        assert_eq!(parse_at("2026-10-08T09:30:00z"), Ok(base));
        assert_eq!(parse_at("2026-10-08T09:30Z"), Ok(base));
    }

    #[test]
    fn parse_at_applies_numeric_offsets() {
        assert_eq!(
            parse_at("2026-10-08T09:30:00+02:00"),
            Ok(ms(2026, 10, 8, 7, 30))
        );
        assert_eq!(
            parse_at("2026-10-08T09:30:00-05:30"),
            Ok(ms(2026, 10, 8, 15, 0))
        );
    }

    #[test]
    fn parse_at_keeps_fraction_to_milliseconds() {
        let base = ms(2026, 10, 8, 9, 30);
        assert_eq!(parse_at("2026-10-08T09:30:00.250Z"), Ok(base + 250));
        assert_eq!(parse_at("2026-10-08T09:30:00.5Z"), Ok(base + 500));
        assert_eq!(parse_at("2026-10-08T09:30:00.123456Z"), Ok(base + 123));
    }

    #[test]
    fn parse_at_accepts_a_space_for_the_t() {
        assert_eq!(parse_at("2026-10-08 09:30:00Z"), Ok(ms(2026, 10, 8, 9, 30)));
    }

    #[test]
    fn parse_at_reads_leap_second_as_59() {
        assert_eq!(
            parse_at("2026-10-08T09:30:60Z"),
            Ok(ms(2026, 10, 8, 9, 30) + 59_000)
        );
    }

    #[test]
    fn parse_at_checks_calendar_and_clock_ranges() {
        for bad in [
            "2026-13-01T00:00Z",
            "2026-00-10T00:00Z",
            "2026-04-31T00:00Z",
            "2026-10-00T00:00Z",
            "2026-10-08T24:00Z",
            "2026-10-08T09:60Z",
            "2026-10-08T09:30:61Z",
        ] {
            assert_eq!(parse_at(bad), Err(not_a_time(bad)), "{bad}");
        }
    }

    #[test]
    fn parse_at_knows_leap_years() {
        assert_eq!(parse_at("2024-02-29T00:00Z"), Ok(ms(2024, 2, 29, 0, 0)));
        assert_eq!(parse_at("2000-02-29T00:00Z"), Ok(ms(2000, 2, 29, 0, 0)));
        assert_eq!(
            parse_at("2025-02-29T00:00Z"),
            Err(not_a_time("2025-02-29T00:00Z"))
        );
        assert_eq!(
            parse_at("2100-02-29T00:00Z"),
            Err(not_a_time("2100-02-29T00:00Z"))
        );
    }

    #[test]
    fn parse_at_rejects_malformed_text() {
        for bad in [
            "",
            "-",
            "yesterday",
            "2026-10-08",
            "2026-10-08T09:30:00",
            "2026-10-08T09:30:00+0200",
            "2026-10-08T09:30:00+02:60",
            "2026-10-08T09:30:00+24:00",
            "2026-10-08T09:30:00Zjunk",
            "2026-10-08T09:30:00.Z",
            "99999999999999999999",
        ] {
            assert_eq!(parse_at(bad), Err(not_a_time(bad)), "{bad}");
        }
    }

    #[test]
    fn parse_spec_reads_at() {
        assert_eq!(
            parse_spec("at:2026-10-08T09:30:00Z"),
            Ok(TimerSpec::At {
                at_ms: ms(2026, 10, 8, 9, 30)
            })
        );
        assert_eq!(parse_spec("at:5000"), Ok(TimerSpec::At { at_ms: 5_000 }));
        assert_eq!(parse_spec("at:tomorrow"), Err(not_a_time("tomorrow")));
    }

    #[test]
    fn parse_spec_reads_every_with_units() {
        assert_eq!(parse_spec("every:90"), Ok(every(90)));
        assert_eq!(parse_spec("every:30s"), Ok(every(30)));
        assert_eq!(parse_spec("every:5m"), Ok(every(300)));
        assert_eq!(parse_spec("every:2h"), Ok(every(7_200)));
        assert_eq!(parse_spec("every:1d"), Ok(every(86_400)));
    }

    #[test]
    fn parse_spec_rejects_bad_intervals() {
        for bad in [
            "every:",
            "every:1.5h",
            "every:-5",
            "every:+5",
            "every:5ms",
            "every:h",
            "every:999999999999999999999",
            "every:99999999999999999999h",
        ] {
            assert!(parse_spec(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn parse_spec_reads_cron_and_rejects_unknown_prefixes() {
        assert_eq!(
            parse_spec("cron:0 9 * * mon-fri"),
            Ok(TimerSpec::Cron {
                expr: "0 9 * * mon-fri".to_string()
            })
        );
        for bad in ["", "weekly:1", "AT:5000", "0 9 * * *"] {
            assert_eq!(parse_spec(bad), Err(SCHEDULE_HINT.to_string()), "{bad}");
        }
    }

    #[test]
    fn validate_enforces_the_minimum_interval() {
        let too_short = Err("every must be at least 60 seconds".to_string());
        assert_eq!(validate(&every(59)), too_short);
        assert_eq!(validate(&every(0)), too_short);
        assert_eq!(validate(&every(60)), Ok(()));
    }

    #[test]
    fn validate_rejects_a_period_whose_milliseconds_overflow() {
        // 9_223_372_036_854_775 s is the largest whole-second period whose millisecond value fits in i64.
        assert_eq!(validate(&every(9_223_372_036_854_775)), Ok(()));
        let too_large = Err("every is too large".to_string());
        assert_eq!(validate(&every(9_223_372_036_854_776)), too_large);
        assert_eq!(validate(&every(u64::MAX)), too_large);
    }

    #[test]
    fn validate_checks_cron_and_accepts_any_at() {
        assert_eq!(validate(&TimerSpec::At { at_ms: -1 }), Ok(()));
        assert_eq!(
            validate(&TimerSpec::Cron {
                expr: "0 9 * * mon-fri".to_string()
            }),
            Ok(())
        );
        assert_eq!(
            validate(&TimerSpec::Cron {
                expr: "61 * * * *".to_string()
            }),
            Err("cron minute field: 61 is out of range 0-59".to_string())
        );
    }

    #[test]
    fn first_fire_per_kind() {
        let created = ms(2025, 1, 1, 12, 0);
        assert_eq!(
            first_fire(
                &TimerSpec::At {
                    at_ms: created - 1_000
                },
                created
            ),
            Some(created - 1_000)
        );
        assert_eq!(first_fire(&every(60), created), Some(created + 60_000));
        let anchored = TimerSpec::Every {
            every_s: 60,
            start_ms: Some(5),
        };
        assert_eq!(first_fire(&anchored, created), Some(5));
        let daily = TimerSpec::Cron {
            expr: "0 0 * * *".to_string(),
        };
        assert_eq!(first_fire(&daily, created), Some(ms(2025, 1, 2, 0, 0)));
        let broken = TimerSpec::Cron {
            expr: "bogus".to_string(),
        };
        assert_eq!(first_fire(&broken, created), None);
        assert_eq!(first_fire(&every(u64::MAX), created), None);
    }

    #[test]
    fn next_fire_at_is_none() {
        assert_eq!(next_fire(&TimerSpec::At { at_ms: 0 }, 0, 10), None);
    }

    #[test]
    fn next_fire_every_coalesces_missed_fires() {
        let t = ms(2026, 10, 8, 9, 30);
        let spec = every(60);
        assert_eq!(
            next_fire(&spec, t, t + 10 * 60_000 + 5_000),
            Some(t + 11 * 60_000)
        );
        // Exactly on a grid point the next fire is one period later: strictly after now.
        assert_eq!(next_fire(&spec, t, t + 10 * 60_000), Some(t + 11 * 60_000));
        assert_eq!(
            next_fire(&spec, t, t + 10 * 60_000 + 59_999),
            Some(t + 11 * 60_000)
        );
    }

    #[test]
    fn next_fire_every_keeps_the_start_anchor() {
        let t = ms(2026, 10, 8, 9, 30);
        let spec = TimerSpec::Every {
            every_s: 60,
            start_ms: Some(t - 30_000),
        };
        assert_eq!(next_fire(&spec, t, t + 10_000), Some(t + 30_000));
        assert_eq!(next_fire(&spec, t, t + 30_000), Some(t + 90_000));
    }

    #[test]
    fn next_fire_every_does_not_drift_when_a_fire_is_late() {
        // Scheduled for T and fired 3 ms late: the caller passes the scheduled time, so the grid stays on T + k*60 s.
        let t = ms(2026, 10, 8, 9, 30);
        assert_eq!(next_fire(&every(60), t, t + 3), Some(t + 60_000));
        assert_eq!(
            next_fire(&every(60), t + 60_000, t + 60_003),
            Some(t + 120_000)
        );
    }

    #[test]
    fn next_fire_every_guards_degenerate_periods() {
        assert_eq!(next_fire(&every(0), 0, 10), None);
        assert_eq!(next_fire(&every(u64::MAX), 0, 10), None);
    }

    #[test]
    fn next_fire_cron_is_strictly_after_now() {
        let spec = TimerSpec::Cron {
            expr: "0 * * * *".to_string(),
        };
        let ten = ms(2026, 10, 8, 10, 0);
        assert_eq!(next_fire(&spec, ten, ten), Some(ms(2026, 10, 8, 11, 0)));
        assert_eq!(
            next_fire(&spec, ten, ten + 30 * 60_000),
            Some(ms(2026, 10, 8, 11, 0))
        );
    }

    #[test]
    fn describe_renders_each_kind() {
        let at = TimerSpec::At {
            at_ms: ms(2026, 10, 8, 9, 30),
        };
        assert_eq!(describe(&at), "at 2026-10-08T09:30:00Z");
        assert_eq!(
            describe(&TimerSpec::At { at_ms: -1_000 }),
            "at 1969-12-31T23:59:59Z"
        );
        assert_eq!(
            describe(&TimerSpec::At {
                at_ms: ms(2026, 10, 8, 9, 30) + 250
            }),
            "at 2026-10-08T09:30:00.250Z"
        );
        assert_eq!(describe(&every(90)), "every 90 s");
        assert_eq!(describe(&every(120)), "every 2 min");
        assert_eq!(describe(&every(300)), "every 5 min");
        assert_eq!(describe(&every(7_200)), "every 2 h");
        assert_eq!(describe(&every(86_400)), "every 1 d");
        let cron = TimerSpec::Cron {
            expr: "0 9 * * mon-fri".to_string(),
        };
        assert_eq!(describe(&cron), "cron 0 9 * * mon-fri (UTC)");
    }
}
