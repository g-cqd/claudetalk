//! work.py's setting parsers (its argparse `type=` functions) and the SIZE grammar (ref-admission section 11,
//! "Parser details"). Errors are work.py's wording after the "--flag: " prefix, with the value shown as a Python repr.

use crate::pyfmt::{py_float, py_int, py_strip, repr_str};

use super::{MemBudget, MemLimit, Qos};

/// "--flag: message" for one setting; the flag is the field name in kebab case.
pub(super) fn flag_error(field: &str, message: &str) -> String {
    format!("--{}: {message}", field.replace('_', "-"))
}

fn repr_error(text: &str, message: &str) -> String {
    format!("{} {message}", repr_str(text))
}

/// work.py `duration`: a finite number, zero or more.
pub(super) fn duration(text: &str) -> Result<f64, String> {
    let value = py_float(text).ok_or_else(|| repr_error(text, "is not a number"))?;
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(repr_error(text, "is not a finite number, zero or more"))
    }
}

/// work.py `positive`: a duration that is not zero.
pub(super) fn positive(text: &str) -> Result<f64, String> {
    let value = duration(text)?;
    if value == 0.0 {
        Err(repr_error(text, "is not greater than zero"))
    } else {
        Ok(value)
    }
}

/// work.py `percent`: a duration below 100.
fn percent(text: &str) -> Result<f64, String> {
    let value = duration(text)?;
    if value < 100.0 {
        Ok(value)
    } else {
        Err(repr_error(text, "is not below 100"))
    }
}

/// work.py `counting(minimum)`: a whole number of at least `minimum`.
fn counting(text: &str, minimum: i64) -> Result<i64, String> {
    let value = py_int(text).ok_or_else(|| repr_error(text, "is not a whole number"))?;
    if value >= minimum {
        Ok(value)
    } else {
        Err(repr_error(text, &format!("is less than {minimum}")))
    }
}

/// argparse `choices`, in agora's wording.
fn choice(text: &str, options: &[&str]) -> String {
    format!("{} is not one of {}", repr_str(text), options.join(", "))
}

/// Python `round(float(number) * 1024 ** index("KMGT"))`, rejected when zero or too large for u64.
pub(super) fn size_kib(text: &str, otherwise: &str) -> Result<u64, String> {
    let kib = size_value(text).ok_or_else(|| repr_error(text, otherwise))?;
    if kib <= 0.0 {
        return Err(repr_error(text, "is not greater than zero"));
    }
    if kib >= 9.0e18 {
        return Err(repr_error(text, "is too large"));
    }
    Ok(kib as u64)
}

/// The SIZE regex `(\d+(?:\.\d+)?)\s*([KMGT])(?:i?B)?` (case-insensitive, fullmatch after strip), in KiB, rounded
/// half to even. None when the text does not match.
fn size_value(text: &str) -> Option<f64> {
    let body = py_strip(text);
    let int_len = count_digits(body);
    if int_len == 0 {
        return None;
    }
    let mut number_len = int_len;
    if let Some(after_dot) = body.get(int_len..)?.strip_prefix('.') {
        let frac_len = count_digits(after_dot);
        if frac_len == 0 {
            return None;
        }
        number_len += 1 + frac_len;
    }
    let number: f64 = body.get(..number_len)?.parse().ok()?;
    let rest = body
        .get(number_len..)?
        .trim_start_matches(char::is_whitespace);
    let mut chars = rest.chars();
    let scale = match chars.next()?.to_ascii_uppercase() {
        'K' => 1.0,
        'M' => 1024.0,
        'G' => 1024.0 * 1024.0,
        'T' => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    if !matches!(
        chars.as_str().to_ascii_lowercase().as_str(),
        "" | "b" | "ib"
    ) {
        return None;
    }
    Some((number * scale).round_ties_even())
}

fn count_digits(text: &str) -> usize {
    text.chars().take_while(char::is_ascii_digit).count()
}

pub(super) fn cpu_budget(text: &str) -> Result<Option<f64>, String> {
    if text == "auto" {
        Ok(None)
    } else {
        positive(text).map(Some)
    }
}

pub(super) fn capacity(text: &str) -> Result<Option<f64>, String> {
    if matches!(text, "off" | "auto") {
        Ok(None)
    } else {
        positive(text).map(Some)
    }
}

pub(super) fn memory_budget(text: &str) -> Result<MemBudget, String> {
    match text {
        "auto" => Ok(MemBudget::Auto),
        "off" => Ok(MemBudget::Off),
        _ => size_kib(
            text,
            "is not a size with its unit, as 14G or 14336M, nor auto or off",
        )
        .map(MemBudget::Kib),
    }
}

pub(super) fn idle_min(text: &str) -> Result<f64, String> {
    percent(text)
}

pub(super) fn memory_limit(text: &str) -> Result<MemLimit, String> {
    match text {
        "warning" => Ok(MemLimit::Warning),
        "critical" => Ok(MemLimit::Critical),
        "off" => Ok(MemLimit::Off),
        _ => Err(choice(text, &["warning", "critical", "off"])),
    }
}

pub(super) fn log_limit(text: &str) -> Result<u64, String> {
    let value = counting(text, 1)?;
    u64::try_from(value).map_err(|_| repr_error(text, "is too large"))
}

pub(super) fn job_qos(text: &str) -> Result<Qos, String> {
    match text {
        "utility" => Ok(Qos::Utility),
        "default" => Ok(Qos::Default),
        "background" => Ok(Qos::Background),
        _ => Err(choice(text, &["utility", "default", "background"])),
    }
}

pub(super) fn self_accept(text: &str) -> Result<bool, String> {
    match text {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(repr_error(text, "is not true or false")),
    }
}

pub(super) fn timer_quota(text: &str) -> Result<u32, String> {
    let value = counting(text, 1)?;
    u32::try_from(value).map_err(|_| repr_error(text, "is too large"))
}

/// The per-identity waiting cap: a whole number of jobs, at least 1 (the same rule as the timer quota).
pub(super) fn max_waiting(text: &str) -> Result<u32, String> {
    timer_quota(text)
}

/// The lowest role that may run or accept commands: one of the roles in `RUN_ROLES`.
pub(super) fn run_role(text: &str) -> Result<String, String> {
    if super::RUN_ROLES.contains(&text) {
        Ok(text.to_owned())
    } else {
        Err(choice(text, &super::RUN_ROLES))
    }
}

/// A reconciler floor in KiB: "off" gives None.
pub(super) fn size_floor(text: &str) -> Result<Option<u64>, String> {
    if text == "off" {
        Ok(None)
    } else {
        size_kib(
            text,
            "is not a size with its unit, as 14G or 14336M, nor off",
        )
        .map(Some)
    }
}

/// The idle floor in percent: "off" gives None.
pub(super) fn idle_floor(text: &str) -> Result<Option<f64>, String> {
    if text == "off" {
        Ok(None)
    } else {
        percent(text).map(Some)
    }
}

/// The reconcile interval in seconds: finite and one second or more.
pub(super) fn reconcile_interval(text: &str) -> Result<f64, String> {
    let value = py_float(text).ok_or_else(|| repr_error(text, "is not a number"))?;
    if value.is_finite() && value >= 1.0 {
        Ok(value)
    } else {
        Err(repr_error(
            text,
            "is not a finite number, one second or more",
        ))
    }
}
