//! CPU time and peak resident size of the children this process has reaped, from `getrusage(RUSAGE_CHILDREN)`.
//!
//! This is the one audited unsafe module of agora-run. The supervisor has exactly one child, so the counters
//! describe the job; the caller takes a baseline before the launch and subtracts it after the reap. The counters
//! are process-wide, so a process that reaps other children concurrently (only the in-process tests do) would see
//! their usage too.

use std::io;

/// What the reaped children have used so far.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChildUsage {
    /// User plus system CPU time, in microseconds. Monotonic, so the caller subtracts a baseline in integers; a float
    /// difference would not survive the JSON round trip of status.json exactly.
    pub cpu_micros: u64,
    /// The largest resident set of any reaped child, in KiB. This is a running maximum, not a sum.
    pub max_rss_kib: u64,
}

/// Reads the usage of the children reaped so far. An error is returned, not guessed at.
pub fn children() -> io::Result<ChildUsage> {
    // SAFETY: an all-zero `rusage` is a valid value: every field is an integer or a `timeval` of integers.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `&mut ru` is a valid, aligned, writable `rusage` that lives for the whole call. getrusage writes
    // only into that struct, and only when it returns 0. RUSAGE_CHILDREN is a documented constant.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ChildUsage {
        cpu_micros: timeval_micros(ru.ru_utime).saturating_add(timeval_micros(ru.ru_stime)),
        max_rss_kib: max_rss_kib(ru.ru_maxrss),
    })
}

/// A `timeval` as microseconds. Negative fields (never produced by the kernel) read as zero.
fn timeval_micros(time: libc::timeval) -> u64 {
    let secs = u64::try_from(time.tv_sec).unwrap_or(0);
    let micros = u64::try_from(time.tv_usec).unwrap_or(0);
    secs.saturating_mul(1_000_000).saturating_add(micros)
}

/// `ru_maxrss` is in bytes on macOS and in KiB on Linux. A negative value (never produced) reads as zero.
fn max_rss_kib(maxrss: libc::c_long) -> u64 {
    let raw = u64::try_from(maxrss).unwrap_or(0);
    if cfg!(target_os = "macos") {
        raw / 1024
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_rss_is_bytes_on_macos_and_kib_elsewhere() {
        let expected = if cfg!(target_os = "macos") {
            4096 / 1024
        } else {
            4096
        };
        assert_eq!(max_rss_kib(4096), expected);
        assert_eq!(max_rss_kib(-1), 0);
    }

    #[test]
    fn timeval_adds_seconds_and_microseconds() {
        let time = libc::timeval {
            tv_sec: 2,
            tv_usec: 500_000,
        };
        assert_eq!(timeval_micros(time), 2_500_000);
    }
}
