//! Machine probes (ref-admission section 3): real probes and a test double.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use agora_sched::admit::{MachineSnapshot, MemLevel, Signals};
use sysinfo::System;

pub trait Machine: Send + Sync {
    fn snapshot(&self) -> MachineSnapshot;
}

/// Real probes: effective cpus (count, Linux affinity + cgroup v2); idle% from CPU tick deltas between consecutive
/// snapshot() calls (first call: idle None) - macOS via sysinfo global CPU usage is acceptable; memory level macOS
/// `sysctl -n kern.memorystatus_vm_pressure_level` (1/2/4) run as a child with a 2 s timeout (cached 1 s), Linux
/// MemAvailable share; memory used (sysinfo used memory); load1 (sysinfo); load_gate = Linux; disk_free_kib via
/// rustix statvfs of `home`.
pub struct RealMachine {
    home: PathBuf,
    /// Effective CPU count, read once in `new`.
    cpus: f64,
    state: Mutex<RealState>,
}

/// Probe state kept between snapshots.
struct RealState {
    sys: System,
    /// When CPU usage was last refreshed; None before the first refresh (sysinfo needs two to report usage).
    last_cpu_refresh: Option<Instant>,
    /// The idle reading taken at the last refresh, repeated by calls that come sooner than the minimum interval.
    last_idle: Option<f64>,
    /// The last macOS memory-pressure reading and when it was taken (cached for one second).
    #[cfg(target_os = "macos")]
    pressure: Option<(Instant, Option<MemLevel>)>,
}

impl RealMachine {
    /// Probes for a daemon whose store lives under `home` (the disk probe measures that filesystem).
    pub fn new(home: PathBuf) -> RealMachine {
        RealMachine {
            home,
            cpus: effective_cpus(),
            state: Mutex::new(RealState {
                sys: System::new(),
                last_cpu_refresh: None,
                last_idle: None,
                #[cfg(target_os = "macos")]
                pressure: None,
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, RealState> {
        // A poisoned lock only means a probe panicked mid-refresh; the cached values are still plain data.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Machine for RealMachine {
    fn snapshot(&self) -> MachineSnapshot {
        let mut st = self.state();
        st.sys.refresh_memory();
        let total_kib = st.sys.total_memory() / 1024;
        MachineSnapshot {
            cpus: self.cpus,
            total_memory_kib: (total_kib > 0).then_some(total_kib),
            disk_free_kib: disk_free_kib(&self.home),
            signals: Some(Signals {
                idle_pct: idle_pct(&mut st),
                memory_level: memory_level(&mut st),
                load1: load1(),
                load_gate: cfg!(target_os = "linux"),
                memory_used_kib: Some(used_memory_kib(&st.sys)),
            }),
        }
    }
}

/// Idle percentage from sysinfo's global CPU usage, measured since the previous refresh. sysinfo measures nothing
/// over a shorter span than `MINIMUM_CPU_UPDATE_INTERVAL`, so a call that comes sooner repeats the last reading. The
/// first refresh only primes sysinfo, so the first snapshot of a process reports None.
fn idle_pct(st: &mut RealState) -> Option<f64> {
    idle_pct_at(st, Instant::now())
}

/// `idle_pct` with the time of the call given, so the interval logic can be tested without sleeping.
fn idle_pct_at(st: &mut RealState, now: Instant) -> Option<f64> {
    let due = st
        .last_cpu_refresh
        .is_none_or(|at| now.saturating_duration_since(at) >= sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    if !due {
        return st.last_idle;
    }
    let primed = st.last_cpu_refresh.is_some();
    st.sys.refresh_cpu_usage();
    st.last_cpu_refresh = Some(now);
    let idle = primed
        .then(|| 100.0 - f64::from(st.sys.global_cpu_usage()))
        .filter(|idle| idle.is_finite())
        .map(|idle| idle.clamp(0.0, 100.0));
    st.last_idle = idle;
    idle
}

/// Memory in use in KiB. Linux: MemTotal - MemAvailable from /proc/meminfo. Elsewhere, or when that file cannot be
/// read: sysinfo's used memory, which on macOS is internal - purgeable + wired + compressor pages (about active +
/// wired + compressed).
fn used_memory_kib(sys: &System) -> u64 {
    linux_used_kib().unwrap_or_else(|| sys.used_memory() / 1024)
}

#[cfg(target_os = "linux")]
fn linux_used_kib() -> Option<u64> {
    read_meminfo().and_then(|text| used_kib_from_meminfo(&text))
}

#[cfg(not(target_os = "linux"))]
fn linux_used_kib() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn read_meminfo() -> Option<String> {
    std::fs::read_to_string("/proc/meminfo").ok()
}

/// macOS: `kern.memorystatus_vm_pressure_level` via sysctl, cached for one second.
#[cfg(target_os = "macos")]
fn memory_level(st: &mut RealState) -> Option<MemLevel> {
    const CACHE: std::time::Duration = std::time::Duration::from_secs(1);
    if let Some((_, level)) = st.pressure.filter(|(taken, _)| taken.elapsed() < CACHE) {
        return level;
    }
    let level = run_bounded(
        "/usr/sbin/sysctl",
        &["-n", "kern.memorystatus_vm_pressure_level"],
        std::time::Duration::from_secs(2),
    )
    .and_then(|out| pressure_from_sysctl(&out));
    st.pressure = Some((Instant::now(), level));
    level
}

/// Linux: the MemAvailable share of MemTotal, from /proc/meminfo.
#[cfg(target_os = "linux")]
fn memory_level(_st: &mut RealState) -> Option<MemLevel> {
    read_meminfo().and_then(|text| level_from_meminfo(&text))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn memory_level(_st: &mut RealState) -> Option<MemLevel> {
    None
}

/// Runs `program args` with stdin and stderr closed and returns its stdout when it exits 0 within `timeout`. A child
/// still running at the deadline is killed and reaped, and gives None.
#[cfg(target_os = "macos")]
fn run_bounded(program: &str, args: &[&str], timeout: std::time::Duration) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    const STEP: std::time::Duration = std::time::Duration::from_millis(10);
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(STEP),
            _ => {
                // Still running at the deadline, or the status could not be read: kill it and reap it.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warning, 4 critical; anything else is unknown.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn pressure_from_sysctl(text: &str) -> Option<MemLevel> {
    match text.trim().parse::<u32>().ok()? {
        1 => Some(MemLevel::Normal),
        2 => Some(MemLevel::Warning),
        4 => Some(MemLevel::Critical),
        _ => None,
    }
}

/// `MemTotal` and `MemAvailable` bands of ref-admission 3.3: share < 0.05 critical, < 0.10 warning, else normal.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn level_from_meminfo(text: &str) -> Option<MemLevel> {
    let total = meminfo_kib(text, "MemTotal").filter(|&kib| kib > 0)?;
    let available = meminfo_kib(text, "MemAvailable")?;
    let share = available as f64 / total as f64;
    Some(if share < 0.05 {
        MemLevel::Critical
    } else if share < 0.10 {
        MemLevel::Warning
    } else {
        MemLevel::Normal
    })
}

/// MemTotal - MemAvailable in KiB, not below zero.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn used_kib_from_meminfo(text: &str) -> Option<u64> {
    let total = meminfo_kib(text, "MemTotal")?;
    let available = meminfo_kib(text, "MemAvailable")?;
    Some(total.saturating_sub(available))
}

/// The value in KiB of one `Key:   value kB` line of /proc/meminfo.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn meminfo_kib(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?.strip_prefix(':')?;
        rest.split_whitespace().next()?.parse::<u64>().ok()
    })
}

/// Free space available to unprivileged users on the filesystem holding `home`, in KiB.
fn disk_free_kib(home: &Path) -> Option<u64> {
    let vfs = rustix::fs::statvfs(home).ok()?;
    vfs.f_bavail
        .checked_mul(vfs.f_frsize)
        .map(|bytes| bytes / 1024)
}

/// One-minute load average where sysinfo reports it (Unix).
#[cfg(unix)]
fn load1() -> Option<f64> {
    Some(System::load_average().one)
}

#[cfg(not(unix))]
fn load1() -> Option<f64> {
    None
}

/// Test double: returns the stored snapshot. Default: 8 cpus, 16 GiB total, 100 GiB disk free,
/// Signals { idle 90, Normal, load None, gate false, used 4 GiB }.
pub struct FakeMachine {
    snap: Mutex<MachineSnapshot>,
}

impl FakeMachine {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Arc<FakeMachine> {
        Arc::new(FakeMachine {
            snap: Mutex::new(MachineSnapshot {
                cpus: 8.0,
                total_memory_kib: Some(16 * 1024 * 1024),
                disk_free_kib: Some(100 * 1024 * 1024),
                signals: Some(Signals {
                    idle_pct: Some(90.0),
                    memory_level: Some(MemLevel::Normal),
                    load1: None,
                    load_gate: false,
                    memory_used_kib: Some(4 * 1024 * 1024),
                }),
            }),
        })
    }

    pub fn set(&self, s: MachineSnapshot) {
        *self.lock() = s;
    }

    pub fn update(&self, f: impl FnOnce(&mut MachineSnapshot)) {
        f(&mut self.lock());
    }

    /// A poisoned lock only means a test closure panicked mid-update; the snapshot itself is still plain data.
    fn lock(&self) -> MutexGuard<'_, MachineSnapshot> {
        self.snap.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Machine for FakeMachine {
    fn snapshot(&self) -> MachineSnapshot {
        self.lock().clone()
    }
}

/// Effective CPU count: the online count, bounded by the Linux affinity mask and cgroup v2 quota.
pub fn effective_cpus() -> f64 {
    // available_parallelism already applies the Linux affinity mask (and a cgroup quota, rounded up); the quota walk
    // below keeps a fractional quota exact.
    let host = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    cpu_capacity(host, cgroup_quota())
}

/// The smaller of the host count and the quota, as a fraction when the quota is fractional. A quota with a
/// non-positive amount or period is no limit.
fn cpu_capacity(host: usize, quota: Option<(i64, i64)>) -> f64 {
    let host = host as f64;
    match quota {
        Some((amount, period)) if amount > 0 && period > 0 => ratio((amount, period)).min(host),
        _ => host,
    }
}

fn ratio((amount, period): (i64, i64)) -> f64 {
    amount as f64 / period as f64
}

/// Linux: the cgroup v2 quota that bounds this process, as (amount, period) in microseconds.
#[cfg(target_os = "linux")]
fn cgroup_quota() -> Option<(i64, i64)> {
    let proc_cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let relative = cgroup_v2_relative(&proc_cgroup)?;
    cgroup_quota_in(relative, Path::new("/sys/fs/cgroup"), |file| {
        std::fs::read_to_string(file).ok()
    })
}

#[cfg(not(target_os = "linux"))]
fn cgroup_quota() -> Option<(i64, i64)> {
    None
}

/// The cgroup v2 path of `/proc/self/cgroup` (its `0::` line) without the leading slash. A v1-only host has no such
/// line, so None.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn cgroup_v2_relative(proc_cgroup: &str) -> Option<&str> {
    proc_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| path.trim_start_matches('/'))
}

/// Walks from `root/relative` up to and including `root`, reads `cpu.max` in each directory, and returns the pair
/// with the smallest quota/period. Files that are unreadable, say `max`, or not positive are skipped.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn cgroup_quota_in(
    relative: &str,
    root: &Path,
    read: impl Fn(&Path) -> Option<String>,
) -> Option<(i64, i64)> {
    let mut dirs = vec![root.to_path_buf()];
    let mut leaf = root.to_path_buf();
    for component in Path::new(relative).components() {
        if let std::path::Component::Normal(name) = component {
            leaf.push(name);
            dirs.push(leaf.clone());
        }
    }
    dirs.iter()
        .filter_map(|dir| read(&dir.join("cpu.max")).and_then(|text| parse_cpu_max(&text)))
        .min_by(|a, b| ratio(*a).total_cmp(&ratio(*b)))
}

/// A `cpu.max` file: `max PERIOD` (no limit) or `QUOTA PERIOD`. Only a positive pair is a limit.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_cpu_max(text: &str) -> Option<(i64, i64)> {
    let mut fields = text.split_whitespace();
    let (quota, period) = (fields.next()?, fields.next()?);
    if fields.next().is_some() {
        return None;
    }
    let quota = quota.parse::<i64>().ok()?;
    let period = period.parse::<i64>().ok()?;
    (quota > 0 && period > 0).then_some((quota, period))
}

#[cfg(test)]
mod tests;
