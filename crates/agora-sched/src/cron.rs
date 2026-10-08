//! 5-field UTC cron expressions. No chrono: civil-from-days / days-from-civil on UTC epoch milliseconds.

const MIN_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
/// Search horizon of `next_after`: eight 366-day years, so a Feb 29 skipped by a non-leap 2100 is still found.
const HORIZON_MS: i64 = 8 * 366 * DAY_MS;
/// Years past this bound have no representation in i64 milliseconds; clamping keeps the arithmetic overflow-free.
const MAX_YEAR_ABS: i64 = 300_000_000;

const FIELDS_MSG: &str =
    "a cron expression has 5 fields: minute hour day-of-month month day-of-week";

const MONTH_NAMES: &[(&str, u32)] = &[
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("may", 5),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];
const DOW_NAMES: &[(&str, u32)] = &[
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];

/// One cron field: its name for error messages, its inclusive value range and its optional names.
struct Field {
    name: &'static str,
    lo: u32,
    hi: u32,
    names: &'static [(&'static str, u32)],
}

const MINUTE: Field = Field {
    name: "minute",
    lo: 0,
    hi: 59,
    names: &[],
};
const HOUR: Field = Field {
    name: "hour",
    lo: 0,
    hi: 23,
    names: &[],
};
const DOM: Field = Field {
    name: "day-of-month",
    lo: 1,
    hi: 31,
    names: &[],
};
const MONTH: Field = Field {
    name: "month",
    lo: 1,
    hi: 12,
    names: MONTH_NAMES,
};
/// Day-of-week accepts 0-7; both 0 and 7 are Sunday.
const DOW: Field = Field {
    name: "day-of-week",
    lo: 0,
    hi: 7,
    names: DOW_NAMES,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    pub minutes: u64,
    pub hours: u32,
    pub dom: u32,
    pub months: u16,
    pub dow: u8,
    /// The day-of-month field starts with `*` (so `*/2` counts as star, as in Vixie cron).
    pub dom_any: bool,
    /// The day-of-week field starts with `*` (so `*/2` counts as star, as in Vixie cron).
    pub dow_any: bool,
}

impl Cron {
    /// 5 fields: minute hour day-of-month month day-of-week; "*", "a", "a-b", "*/n", "a-b/n", lists; month names
    /// jan..dec, weekday names sun..sat (case-insensitive); dow 7 = sunday. Errors name the field.
    pub fn parse(expr: &str) -> Result<Cron, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        let &[minute, hour, dom, month, dow] = fields.as_slice() else {
            return Err(FIELDS_MSG.to_string());
        };
        let minutes = parse_field(minute, &MINUTE)?;
        let hours = parse_field(hour, &HOUR)?;
        let dom_bits = parse_field(dom, &DOM)?;
        let months = parse_field(month, &MONTH)?;
        let dow_bits = parse_field(dow, &DOW)?;
        // Fold Sunday-as-7 onto bit 0. Bits above 7 cannot occur: every value was range-checked.
        let dow_bits = (dow_bits | (dow_bits >> 7)) & 0x7f;
        Ok(Cron {
            minutes,
            hours: narrow(hours),
            dom: narrow(dom_bits),
            months: narrow(months),
            dow: narrow(dow_bits),
            dom_any: dom.starts_with('*'),
            dow_any: dow.starts_with('*'),
        })
    }

    /// First minute boundary strictly after `after_ms` that matches (Vixie rule: if either dom or dow starts with `*`
    /// both must match; otherwise either matching suffices). None if nothing matches within 8 years (e.g. Feb 30), or
    /// if the next match would lie beyond i64 milliseconds.
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        // The first minute boundary strictly after `after_ms`; rounding up by the remainder cannot underflow.
        let mut t = after_ms.checked_add(MIN_MS - after_ms.rem_euclid(MIN_MS))?;
        // The search stops at the horizon, clamped to i64::MAX so the bound itself never overflows.
        let horizon = t.checked_add(HORIZON_MS).unwrap_or(i64::MAX);
        // Every step moves `t` strictly forward, so the loop ends at a match, past the horizon, or on overflow.
        while t <= horizon {
            let (year, month, day, hour, minute, weekday) = civil_from_ms(t);
            // `t` is a minute boundary, so these are its offsets from the day and hour starts.
            let in_day = t.rem_euclid(DAY_MS);
            let in_hour = i64::from(minute) * MIN_MS;
            if (self.months & (1 << month)) == 0 {
                t = next_month_start(year, month)?;
            } else if !self.day_matches(day, weekday) {
                t = t.checked_add(DAY_MS - in_day)?;
            } else if (self.hours & (1 << hour)) == 0 {
                t = match next_set(u64::from(self.hours), hour) {
                    Some(next) => t.checked_add(i64::from(next - hour) * HOUR_MS - in_hour)?,
                    None => t.checked_add(DAY_MS - in_day)?,
                };
            } else if (self.minutes & (1 << minute)) == 0 {
                t = match next_set(self.minutes, minute) {
                    Some(next) => t.checked_add(i64::from(next - minute) * MIN_MS)?,
                    None => t.checked_add(HOUR_MS - in_hour)?,
                };
            } else {
                return Some(t);
            }
        }
        None
    }

    /// Vixie day rule: when either field starts with `*` a day must match on both dom and dow; otherwise on dom or dow.
    fn day_matches(&self, day: u32, weekday: u32) -> bool {
        let dom_ok = (self.dom & (1 << day)) != 0;
        let dow_ok = (self.dow & (1 << weekday)) != 0;
        if self.dom_any || self.dow_any {
            dom_ok && dow_ok
        } else {
            dom_ok || dow_ok
        }
    }
}

/// (year, month 1-12, day 1-31, hour, minute, weekday 0=sun)
pub fn civil_from_ms(ms: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = ms.div_euclid(DAY_MS);
    let in_day = ms.rem_euclid(DAY_MS);
    let (year, month, day) = civil_from_days(days);
    let hour = u32::try_from(in_day / HOUR_MS).unwrap_or_default();
    let minute = u32::try_from((in_day % HOUR_MS) / MIN_MS).unwrap_or_default();
    // 1970-01-01 (day 0) was a Thursday, weekday 4.
    let weekday = u32::try_from((days + 4).rem_euclid(7)).unwrap_or_default();
    (year, month, day, hour, minute, weekday)
}

/// Milliseconds since the epoch of a UTC civil time. Month 1-12 and day 1-31 are the caller's contract; out-of-range
/// values give a deterministic but meaningless result and never panic.
pub fn ms_from_civil(year: i64, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    let year = year.clamp(-MAX_YEAR_ABS, MAX_YEAR_ABS);
    let days = days_from_civil(year, i64::from(month), i64::from(day));
    days.saturating_mul(DAY_MS)
        .saturating_add(i64::from(hour) * HOUR_MS + i64::from(minute) * MIN_MS)
}

/// Howard Hinnant's civil_from_days: day 0 = 1970-01-01 → (year, month 1-12, day 1-31).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        year,
        u32::try_from(month).unwrap_or_default(),
        u32::try_from(day).unwrap_or_default(),
    )
}

/// Howard Hinnant's days_from_civil: the inverse of `civil_from_days`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = (month + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// First day 00:00 of the month after (`year`, `month`); None if that instant is beyond i64 milliseconds.
fn next_month_start(year: i64, month: u32) -> Option<i64> {
    let days = if month >= 12 {
        days_from_civil(year.checked_add(1)?, 1, 1)
    } else {
        days_from_civil(year, i64::from(month) + 1, 1)
    };
    days.checked_mul(DAY_MS)
}

/// Lowest set bit of `mask` at or above `from`.
fn next_set(mask: u64, from: u32) -> Option<u32> {
    let rest = mask.checked_shr(from)?;
    (rest != 0).then(|| from + rest.trailing_zeros())
}

/// Narrows a bitmask whose bits were range-checked by `parse_field`; falls back to zero if that ever fails.
fn narrow<T: TryFrom<u64> + Default>(bits: u64) -> T {
    T::try_from(bits).unwrap_or_default()
}

fn field_err(field: &Field, msg: impl std::fmt::Display) -> String {
    format!("cron {} field: {msg}", field.name)
}

fn all_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Parses one comma-separated field into a bitmask over its value range (bit `v` = value `v`).
fn parse_field(text: &str, field: &Field) -> Result<u64, String> {
    let text = text.to_ascii_lowercase();
    let mut mask = 0u64;
    for item in text.split(',') {
        if item.is_empty() {
            return Err(field_err(field, format!("empty list item in '{text}'")));
        }
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => (range, Some(parse_step(step, field)?)),
            None => (item, None),
        };
        let (lo, hi) = if range == "*" {
            (field.lo, field.hi)
        } else if let Some((from, to)) = range.split_once('-') {
            let from = parse_value(from, field)?;
            let to = parse_value(to, field)?;
            if from > to {
                return Err(field_err(field, format!("range {from}-{to} is backwards")));
            }
            (from, to)
        } else {
            let from = parse_value(range, field)?;
            // `a/n` runs from a to the field maximum.
            (from, if step.is_some() { field.hi } else { from })
        };
        let step = step.unwrap_or(1);
        let mut value = lo;
        while value <= hi {
            mask |= 1u64 << value;
            value = value.saturating_add(step);
        }
    }
    Ok(mask)
}

fn parse_step(text: &str, field: &Field) -> Result<u32, String> {
    let step = if all_digits(text) {
        text.parse::<u32>().ok()
    } else {
        None
    };
    match step {
        Some(0) => Err(field_err(field, "step must be at least 1")),
        Some(step) => Ok(step),
        None => Err(field_err(field, format!("step '{text}' is not a number"))),
    }
}

/// A number inside the field's range, or a name of that field (case already folded).
fn parse_value(text: &str, field: &Field) -> Result<u32, String> {
    if all_digits(text) {
        return text
            .parse::<u32>()
            .ok()
            .filter(|v| (field.lo..=field.hi).contains(v))
            .ok_or_else(|| {
                field_err(
                    field,
                    format!("{text} is out of range {}-{}", field.lo, field.hi),
                )
            });
    }
    field
        .names
        .iter()
        .find(|(name, _)| *name == text)
        .map(|&(_, value)| value)
        .ok_or_else(|| field_err(field, format!("'{text}' is not a valid value")))
}

#[cfg(test)]
mod tests;
