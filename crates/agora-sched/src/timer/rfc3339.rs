//! RFC 3339 instants for `parse_at`: a bounds-checked byte cursor, no chrono, no panics on any input.

use crate::cron::ms_from_civil;

/// A byte cursor over time text. Every step is bounds-checked, so no input can panic.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    /// Consumes one byte if it is one of `options`.
    fn accept(&mut self, options: &[u8]) -> bool {
        match self.bytes.get(self.pos) {
            Some(byte) if options.contains(byte) => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    fn expect(&mut self, options: &[u8]) -> Option<()> {
        self.accept(options).then_some(())
    }

    /// Exactly `width` ASCII digits as a number.
    fn digits(&mut self, width: usize) -> Option<u32> {
        let end = self.pos.checked_add(width)?;
        let digits = self.bytes.get(self.pos..end)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.pos = end;
        Some(
            digits
                .iter()
                .fold(0, |acc, &b| acc * 10 + u32::from(b - b'0')),
        )
    }

    /// One or more digits after the '.', as milliseconds; digits past the third are truncated.
    fn fraction_ms(&mut self) -> Option<u32> {
        let mut ms = 0;
        let mut count = 0usize;
        while let Some(&b) = self.bytes.get(self.pos) {
            if !b.is_ascii_digit() {
                break;
            }
            if count < 3 {
                ms = ms * 10 + u32::from(b - b'0');
            }
            count += 1;
            self.pos += 1;
        }
        if count == 0 {
            return None;
        }
        for _ in count.min(3)..3 {
            ms *= 10;
        }
        Some(ms)
    }
}

/// RFC 3339 with a space allowed for the 'T', optional seconds and fraction, and a required zone.
/// A leap second (60) reads as 59.
pub(super) fn parse_rfc3339(text: &str) -> Option<i64> {
    let mut c = Cursor {
        bytes: text.as_bytes(),
        pos: 0,
    };
    let year = i64::from(c.digits(4)?);
    c.expect(b"-")?;
    let month = c.digits(2)?;
    c.expect(b"-")?;
    let day = c.digits(2)?;
    c.expect(b"Tt ")?;
    let hour = c.digits(2)?;
    c.expect(b":")?;
    let minute = c.digits(2)?;
    let mut second = 0;
    let mut millis = 0;
    if c.accept(b":") {
        second = c.digits(2)?;
        if c.accept(b".") {
            millis = c.fraction_ms()?;
        }
    }
    let offset_min = if c.accept(b"Zz") {
        0
    } else {
        let sign = if c.accept(b"+") {
            1
        } else {
            c.expect(b"-")?;
            -1
        };
        let offset_hour = c.digits(2)?;
        c.expect(b":")?;
        let offset_minute = c.digits(2)?;
        if offset_hour > 23 || offset_minute > 59 {
            return None;
        }
        sign * i64::from(offset_hour * 60 + offset_minute)
    };
    if c.pos != c.bytes.len() {
        return None;
    }
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let second = second.min(59);
    let local = ms_from_civil(year, month, day, hour, minute);
    Some(local + i64::from(second) * 1_000 + i64::from(millis) - offset_min * 60_000)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        _ => 28,
    }
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}
