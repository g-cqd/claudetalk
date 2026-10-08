use std::collections::HashMap;
use std::path::PathBuf;

use super::*;

fn meminfo(total: u64, available: u64) -> String {
    format!("MemTotal:       {total} kB\nMemAvailable:   {available} kB\nBuffers: 1 kB\n")
}

fn cgroup_files(files: &[(&str, &str)]) -> HashMap<PathBuf, String> {
    let root = Path::new("/sys/fs/cgroup");
    files
        .iter()
        .map(|(rel, text)| (root.join(rel), (*text).to_owned()))
        .collect()
}

#[test]
fn capacity_takes_the_tighter_of_host_and_quota() {
    assert_eq!(cpu_capacity(16, Some((300_000, 100_000))), 3.0);
    assert_eq!(cpu_capacity(2, Some((500_000, 100_000))), 2.0);
}

#[test]
fn capacity_keeps_a_fractional_quota() {
    assert_eq!(cpu_capacity(8, Some((50_000, 100_000))), 0.5);
}

#[test]
fn capacity_ignores_a_quota_that_is_not_positive() {
    assert_eq!(cpu_capacity(4, Some((0, 100_000))), 4.0);
    assert_eq!(cpu_capacity(8, Some((100, 0))), 8.0);
    assert_eq!(cpu_capacity(8, None), 8.0);
}

#[test]
fn cgroup_v2_path_is_the_0_line_and_v1_has_none() {
    assert_eq!(
        cgroup_v2_relative("0::/parent/child\n"),
        Some("parent/child")
    );
    assert_eq!(cgroup_v2_relative("0::/\n"), Some(""));
    assert_eq!(cgroup_v2_relative("12:cpu,cpuacct:/parent\n"), None);
}

#[test]
fn cgroup_walk_takes_the_tightest_ancestor_limit() {
    let files = cgroup_files(&[
        ("cpu.max", "max 100000\n"),
        ("parent/cpu.max", "250000 100000\n"),
        ("parent/child/cpu.max", "max 100000\n"),
    ]);
    let quota = cgroup_quota_in("parent/child", Path::new("/sys/fs/cgroup"), |f| {
        files.get(f).cloned()
    });
    assert_eq!(quota, Some((250_000, 100_000)));
}

#[test]
fn cgroup_walk_without_any_limit_is_none() {
    let files = cgroup_files(&[("cpu.max", "max 100000\n")]);
    let quota = cgroup_quota_in("parent/child", Path::new("/sys/fs/cgroup"), |f| {
        files.get(f).cloned()
    });
    assert_eq!(quota, None);
    assert_eq!(
        cgroup_quota_in("parent", Path::new("/sys/fs/cgroup"), |_| None),
        None
    );
}

#[test]
fn cpu_max_is_a_positive_pair_only() {
    assert_eq!(parse_cpu_max("300000 100000\n"), Some((300_000, 100_000)));
    assert_eq!(parse_cpu_max("max 100000\n"), None);
    assert_eq!(parse_cpu_max("0 100000\n"), None);
    assert_eq!(parse_cpu_max("1 2 3\n"), None);
    assert_eq!(parse_cpu_max("abc 100000\n"), None);
}

#[test]
fn meminfo_level_bands_match_the_thresholds() {
    assert_eq!(
        level_from_meminfo(&meminfo(16_000_000, 4_000_000)),
        Some(MemLevel::Normal)
    );
    assert_eq!(
        level_from_meminfo(&meminfo(16_000_000, 1_200_000)),
        Some(MemLevel::Warning)
    );
    assert_eq!(
        level_from_meminfo(&meminfo(16_000_000, 700_000)),
        Some(MemLevel::Critical)
    );
    // Exactly at a threshold the lower band does not apply: share 0.10 is normal, share 0.05 is warning.
    assert_eq!(
        level_from_meminfo(&meminfo(16_000_000, 1_600_000)),
        Some(MemLevel::Normal)
    );
    assert_eq!(
        level_from_meminfo(&meminfo(16_000_000, 800_000)),
        Some(MemLevel::Warning)
    );
}

#[test]
fn meminfo_level_needs_both_totals() {
    assert_eq!(level_from_meminfo("MemTotal: 16000000 kB\n"), None);
    assert_eq!(
        level_from_meminfo("MemTotal: 0 kB\nMemAvailable: 0 kB\n"),
        None
    );
}

#[test]
fn meminfo_used_is_total_minus_available() {
    assert_eq!(
        used_kib_from_meminfo(&meminfo(16_000_000, 4_000_000)),
        Some(12_000_000)
    );
    assert_eq!(used_kib_from_meminfo(&meminfo(1_000, 5_000)), Some(0));
    assert_eq!(used_kib_from_meminfo("MemTotal: 16000000 kB\n"), None);
}

#[test]
fn meminfo_key_must_match_the_whole_name() {
    assert_eq!(
        meminfo_kib("MemTotalExtra: 7 kB\nMemTotal: 9 kB\n", "MemTotal"),
        Some(9)
    );
}

#[test]
fn sysctl_pressure_values_map_to_levels() {
    assert_eq!(pressure_from_sysctl("1\n"), Some(MemLevel::Normal));
    assert_eq!(pressure_from_sysctl("2\n"), Some(MemLevel::Warning));
    assert_eq!(pressure_from_sysctl("4\n"), Some(MemLevel::Critical));
    assert_eq!(pressure_from_sysctl("3\n"), None);
    assert_eq!(pressure_from_sysctl(""), None);
    assert_eq!(pressure_from_sysctl("high"), None);
}

#[test]
fn effective_cpus_is_positive() {
    // A fractional cgroup quota can make the count below one, so only positivity is certain.
    assert!(effective_cpus() > 0.0);
}

#[test]
fn idle_is_none_while_priming_and_on_a_call_inside_the_interval() {
    let dir = tempfile::tempdir().expect("tempdir");
    let machine = RealMachine::new(dir.path().to_path_buf());
    let mut st = machine.state();
    let t0 = Instant::now();
    assert_eq!(idle_pct_at(&mut st, t0), None, "the priming call");
    assert_eq!(idle_pct_at(&mut st, t0), None, "an immediate second call");
}

#[test]
fn idle_is_refreshed_once_the_interval_has_passed_and_repeated_before_that() {
    let dir = tempfile::tempdir().expect("tempdir");
    let machine = RealMachine::new(dir.path().to_path_buf());
    let mut st = machine.state();
    let t0 = Instant::now();
    assert_eq!(idle_pct_at(&mut st, t0), None);
    let later = t0 + sysinfo::MINIMUM_CPU_UPDATE_INTERVAL;
    let reading = idle_pct_at(&mut st, later).expect("a refresh after the interval reads idle");
    assert!((0.0..=100.0).contains(&reading), "idle {reading}");
    assert_eq!(st.last_cpu_refresh, Some(later));
    let inside = later + std::time::Duration::from_millis(1);
    assert_eq!(idle_pct_at(&mut st, inside), Some(reading));
    assert_eq!(
        st.last_cpu_refresh,
        Some(later),
        "no refresh inside the interval"
    );
}

#[test]
fn real_machine_reports_plausible_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let machine = RealMachine::new(dir.path().to_path_buf());
    let first = machine.snapshot();
    assert_eq!(first.signals.as_ref().and_then(|s| s.idle_pct), None);

    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    let second = machine.snapshot();
    assert!(second.cpus > 0.0);
    assert!(second.total_memory_kib.is_some_and(|kib| kib > 0));
    assert!(second.disk_free_kib.is_some_and(|kib| kib > 0));
    let signals = second.signals.expect("signals");
    let idle = signals.idle_pct.expect("idle after two refreshes");
    assert!((0.0..=100.0).contains(&idle));
    assert!(signals.memory_used_kib.is_some());
    #[cfg(target_os = "macos")]
    assert!(signals.memory_level.is_some());
}

#[cfg(target_os = "macos")]
#[test]
fn bounded_run_returns_the_output_of_a_fast_child() {
    let out = run_bounded("/bin/echo", &["7"], std::time::Duration::from_secs(2));
    assert_eq!(out.as_deref(), Some("7\n"));
}

#[cfg(target_os = "macos")]
#[test]
fn bounded_run_kills_a_child_past_the_timeout() {
    let started = Instant::now();
    let out = run_bounded("/bin/sleep", &["30"], std::time::Duration::from_millis(100));
    assert_eq!(out, None);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}
