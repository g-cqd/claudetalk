use super::*;

#[test]
fn reservation_grows_only_until_the_p90_run_time_passes() {
    let b = Budgets {
        memory_budget_kib: Some(1_000_000),
        ..fixture()
    };
    let sig = Signals {
        memory_used_kib: Some(500_000),
        ..calm()
    };
    let server = |p90: Option<f64>| RunningJob {
        rss_kib: Some(100_000),
        pred_rss_p90_kib: Some(900_000),
        pred_duration_p90_s: p90,
        started_s: Some(NOW - 30.0),
        ..run("server", 1.0, Some(1.0))
    };
    let held = memory_reservation(Some(100_000), &[server(Some(60.0))], &b, Some(&sig), NOW);
    assert!(held.is_some_and(|h| h.cause == Cause::Reserved));
    let later = memory_reservation(
        Some(100_000),
        &[server(Some(60.0))],
        &b,
        Some(&sig),
        NOW + 31.0,
    );
    assert!(later.is_none());
    assert!(memory_reservation(Some(100_000), &[server(None)], &b, Some(&sig), NOW).is_none());
}

#[test]
fn reservation_shows_the_projected_peak_against_the_budget() {
    let b = Budgets {
        memory_budget_kib: Some(1_048_576),
        ..fixture()
    };
    let sig = Signals {
        memory_used_kib: Some(838_861),
        ..calm()
    };
    let running = RunningJob {
        rss_kib: Some(100),
        pred_rss_p90_kib: Some(100),
        pred_duration_p90_s: Some(60.0),
        ..run("r", 1.0, Some(1.0))
    };
    assert_eq!(
        memory_reservation(Some(262_144), &[running], &b, Some(&sig), NOW)
            .map(|h| h.reason)
            .unwrap_or_default(),
        "used memory, the running jobs' predicted growth and this job's predicted peak come to 1.1 GiB, above the memory budget of 1.0 GiB"
    );
    assert!(memory_reservation(Some(262_144), &[], &b, Some(&sig), NOW).is_none());
}

#[test]
fn reservation_is_strict_and_needs_known_estimates() {
    let sig = |used: Option<u64>| Signals {
        memory_used_kib: used,
        ..calm()
    };
    let running = |p90: Option<u64>| RunningJob {
        rss_kib: Some(200_000),
        pred_rss_p90_kib: p90,
        pred_duration_p90_s: Some(60.0),
        ..run("r", 1.0, Some(1.0))
    };
    let b = |budget: u64| Budgets {
        memory_budget_kib: Some(budget),
        ..fixture()
    };
    assert!(
        memory_reservation(
            Some(100_000),
            &[running(Some(400_000))],
            &b(1_100_000),
            Some(&sig(Some(800_000))),
            NOW
        )
        .is_none()
    );
    assert!(
        memory_reservation(
            Some(100_000),
            &[running(Some(400_000))],
            &b(1_099_999),
            Some(&sig(Some(800_000))),
            NOW
        )
        .is_some()
    );
    let plain = RunningJob {
        pred_rss_p90_kib: None,
        ..running(None)
    };
    assert!(memory_reservation(None, &[plain], &b(1000), Some(&sig(Some(900))), NOW).is_none());
    assert!(memory_reservation(Some(500), &[], &b(1000), Some(&sig(None)), NOW).is_none());
}

#[test]
fn used_memory_above_budget_is_a_memory_hold_not_a_reservation() {
    let b = Budgets {
        memory_budget_kib: Some(1_000_000),
        ..fixture()
    };
    let sig = Signals {
        memory_used_kib: Some(1_100_000),
        ..calm()
    };
    let s = WorkSettings::default();
    let running = [RunningJob {
        rss_kib: Some(0),
        ..run("r", 1.0, Some(1.0))
    }];
    let c = WaitingJob {
        pred_rss_p90_kib: Some(10),
        ..job("c", 1.0, Some(1.0), 10.0)
    };
    assert_eq!(
        cause(&admission(&c, &running, &s, &b, Some(&sig), NOW)),
        Some(Cause::Memory)
    );
}

#[test]
fn a_tiny_job_is_still_held_by_idle_cpu() {
    let b = fixture();
    let s = WorkSettings::default();
    let busy = [run("busy", 7.9, None)];
    let idle = Signals {
        idle_pct: Some(5.0),
        ..calm()
    };
    assert_eq!(
        text(admit(&job("t", 1.0, Some(0.2), 10.0), &busy, &s, &b, &idle)),
        "the CPU is 5% idle; jobs start at 10% idle or more"
    );
}

#[test]
fn budgets_follow_the_settings_and_the_machine_memory() {
    let machine = |total: Option<u64>| MachineSnapshot {
        cpus: 8.0,
        total_memory_kib: total,
        disk_free_kib: None,
        signals: None,
    };
    let s = WorkSettings::default();
    let auto = budgets(&s, &machine(Some(16_777_216)));
    assert_eq!(auto.budget, 8.0);
    assert_eq!(auto.memory_budget_kib, Some(14_680_064));
    assert_eq!(auto.load_limit, 12.0);
    assert_eq!(
        budgets(&s, &machine(Some(RESERVE_KIB))).memory_budget_kib,
        None
    );
    assert_eq!(budgets(&s, &machine(None)).memory_budget_kib, None);
    let off = WorkSettings {
        memory_budget: MemBudget::Off,
        cpu_budget: Some(3.0),
        ..WorkSettings::default()
    };
    let off = budgets(&off, &machine(Some(16_777_216)));
    assert_eq!((off.budget, off.memory_budget_kib), (3.0, None));
    let kib = WorkSettings {
        memory_budget: MemBudget::Kib(1_000),
        ..WorkSettings::default()
    };
    assert_eq!(budgets(&kib, &machine(None)).memory_budget_kib, Some(1_000));
    assert_eq!(cores(2.0, None), 2.0);
    assert_eq!(cores(8.0, Some(1.0)), 1.0);
    assert!(is_tiny(0.5) && !is_tiny(0.5000000011));
    assert!(!is_heavy(1.0, 1.0) && is_heavy(1.0000000011, 1.0));
}
