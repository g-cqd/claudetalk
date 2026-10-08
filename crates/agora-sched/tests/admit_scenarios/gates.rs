//! Admission gates and memory reservation, ref-admission sections 3-4 (table A, S12-S22), through `admit::admission`,
//! `admit::pressure` and `admit::memory_reservation`.

use super::*;

// ---- A (continued). Gates and memory ----

#[test]
fn s12_used_memory_gates_against_the_memory_budget() {
    let s = WorkSettings {
        memory_budget: MemBudget::Kib(1_000_000),
        ..settings()
    };
    let cand = wait("c", 1.0, Some(1.0), 10.0);
    let over = Signals {
        memory_used_kib: Some(1_100_000),
        ..calm()
    };
    assert_eq!(
        admit_now(&cand, &[], &s, &machine_with(over)),
        hold(
            Cause::Memory,
            "used memory is 1.0 GiB, above the memory budget of 1.0 GiB"
        ),
    );
    let at_budget = Signals {
        memory_used_kib: Some(1_000_000),
        ..calm()
    };
    assert_eq!(admit_now(&cand, &[], &s, &machine_with(at_budget)), None);
}

#[test]
fn s13_idle_minimum_gates_and_rounds_its_percent_in_the_text() {
    let s = WorkSettings {
        idle_min: 15.0,
        ..settings()
    };
    let cand = wait("c", 1.0, Some(1.0), 10.0);
    let idle = |pct: Option<f64>| {
        machine_with(Signals {
            idle_pct: pct,
            ..calm()
        })
    };
    assert_eq!(
        admit_now(&cand, &[], &s, &idle(Some(8.0))),
        hold(
            Cause::Idle,
            "the CPU is 8% idle; jobs start at 15% idle or more"
        ),
    );
    assert_eq!(admit_now(&cand, &[], &s, &idle(Some(15.0))), None);
    assert_eq!(
        admit_now(&cand, &[], &s, &idle(Some(14.6))),
        hold(
            Cause::Idle,
            "the CPU is 15% idle; jobs start at 15% idle or more"
        ),
    );
    assert_eq!(admit_now(&cand, &[], &s, &idle(None)), None);
}

#[test]
fn s14_load_gate_holds_at_the_limit_on_linux_only() {
    let cand = wait("c", 1.0, Some(1.0), 10.0);
    let loaded = |load: f64, gate: bool| {
        machine_with(Signals {
            load1: Some(load),
            load_gate: gate,
            ..calm()
        })
    };
    // 8 CPUs x load factor 1.5 = a limit of 12.00.
    assert_eq!(
        admit_now(&cand, &[], &settings(), &loaded(92.8, true)),
        hold(Cause::Load, "the load average 92.80 is at or above 12.00"),
    );
    assert_eq!(
        admit_now(&cand, &[], &settings(), &loaded(12.0, true)),
        hold(Cause::Load, "the load average 12.00 is at or above 12.00"),
    );
    assert_eq!(
        admit_now(&cand, &[], &settings(), &loaded(11.99, true)),
        None
    );
    assert_eq!(
        admit_now(&cand, &[], &settings(), &loaded(92.8, false)),
        None
    );
}

#[test]
fn s15_pressure_names_its_first_signal_in_order() {
    let s = WorkSettings {
        memory_budget: MemBudget::Kib(1_000_000),
        ..settings()
    };
    let cand = wait("c", 1.0, Some(1.0), 10.0);
    let all = |level: MemLevel, used: u64, idle: f64| {
        machine_with(Signals {
            memory_level: Some(level),
            memory_used_kib: Some(used),
            idle_pct: Some(idle),
            load1: Some(92.8),
            load_gate: true,
        })
    };
    assert_eq!(
        admit_now(&cand, &[], &s, &all(MemLevel::Warning, 1_100_000, 5.0)),
        hold(
            Cause::Memory,
            "memory pressure is warning; jobs start below warning"
        ),
    );
    assert_eq!(
        admit_now(&cand, &[], &s, &all(MemLevel::Normal, 1_100_000, 5.0)),
        hold(
            Cause::Memory,
            "used memory is 1.0 GiB, above the memory budget of 1.0 GiB"
        ),
    );
    assert_eq!(
        admit_now(&cand, &[], &s, &all(MemLevel::Normal, 900_000, 5.0)),
        hold(
            Cause::Idle,
            "the CPU is 5% idle; jobs start at 10% idle or more"
        ),
    );
    assert_eq!(
        admit_now(&cand, &[], &s, &all(MemLevel::Normal, 900_000, 90.0)),
        hold(Cause::Load, "the load average 92.80 is at or above 12.00"),
    );
}

#[test]
fn s16_cores_beat_capacity_reservation_and_pressure() {
    let s = WorkSettings {
        capacity: Some(4.0),
        memory_budget: MemBudget::Kib(1000),
        ..settings()
    };
    let eight: Vec<RunningJob> = (0..8)
        .map(|i| run(&format!("r{i}"), 1.0, Some(1.0)))
        .collect();
    let m = machine_with(Signals {
        memory_used_kib: Some(2000),
        idle_pct: Some(5.0),
        ..calm()
    });
    assert_eq!(
        admit_now(&wait("c", 2.0, None, 10.0), &eight, &s, &m),
        hold(
            Cause::Cores,
            "predicted 8 of 8 cores are taken and this job needs 2"
        ),
    );
}

#[test]
fn s17_reserves_memory_for_the_candidates_p90_peak() {
    let s = WorkSettings {
        memory_budget: MemBudget::Kib(1_048_576),
        ..settings()
    };
    let m = machine_with(Signals {
        memory_used_kib: Some(838_861),
        ..calm()
    });
    let server = RunningJob {
        rss_kib: Some(100),
        pred_rss_p90_kib: Some(100),
        pred_duration_p90_s: Some(60.0),
        ..run("r", 1.0, Some(1.0))
    };
    let cand = WaitingJob {
        pred_rss_p90_kib: Some(262_144),
        ..wait("c", 1.0, Some(1.0), 10.0)
    };
    assert_eq!(
        admit_now(&cand, std::slice::from_ref(&server), &s, &m),
        hold(
            Cause::Reserved,
            "used memory, the running jobs' predicted growth and this job's predicted peak come to 1.1 GiB, above the memory budget of 1.0 GiB",
        ),
    );
    assert_eq!(admit_now(&cand, &[], &s, &m), None);
}

#[test]
fn s18_a_running_jobs_memory_growth_lapses_at_its_p90_run_time() {
    let s = WorkSettings {
        memory_budget: MemBudget::Kib(1_000_000),
        ..settings()
    };
    let m = machine_with(Signals {
        memory_used_kib: Some(500_000),
        ..calm()
    });
    let server = RunningJob {
        pred_rss_p90_kib: Some(900_000),
        pred_duration_p90_s: Some(60.0),
        rss_kib: Some(100_000),
        started_s: Some(NOW - 30.0),
        ..run("server", 1.0, Some(1.0))
    };
    let cand = WaitingJob {
        pred_rss_p90_kib: Some(100_000),
        ..wait("c", 1.0, Some(1.0), 10.0)
    };
    assert_eq!(
        admit_now(&cand, std::slice::from_ref(&server), &s, &m),
        hold(
            Cause::Reserved,
            "used memory, the running jobs' predicted growth and this job's predicted peak come to 1.3 GiB, above the memory budget of 1.0 GiB",
        ),
    );
    // 31 s on, the server's p90 run time (60 s from 30 s ago) has passed: its growth no longer counts.
    assert_eq!(
        admit_at(&cand, std::slice::from_ref(&server), &s, &m, NOW + 31.0),
        None
    );
    let no_p90_duration = RunningJob {
        pred_duration_p90_s: None,
        ..server
    };
    assert_eq!(
        admit_now(&cand, std::slice::from_ref(&no_p90_duration), &s, &m),
        None
    );
}

#[test]
fn s19_the_memory_reservation_is_exact_at_the_budget() {
    let at_budget = WorkSettings {
        memory_budget: MemBudget::Kib(1_100_000),
        ..settings()
    };
    let below = WorkSettings {
        memory_budget: MemBudget::Kib(1_099_999),
        ..settings()
    };
    let m = machine_with(Signals {
        memory_used_kib: Some(800_000),
        ..calm()
    });
    let grower = RunningJob {
        pred_rss_p90_kib: Some(400_000),
        pred_duration_p90_s: Some(60.0),
        rss_kib: Some(200_000),
        ..run("r", 1.0, Some(1.0))
    };
    let cand = WaitingJob {
        pred_rss_p90_kib: Some(100_000),
        ..wait("c", 1.0, Some(1.0), 10.0)
    };
    assert_eq!(
        admit_now(&cand, std::slice::from_ref(&grower), &at_budget, &m),
        None
    );
    assert_eq!(
        admit_now(&cand, std::slice::from_ref(&grower), &below, &m),
        hold(
            Cause::Reserved,
            "used memory, the running jobs' predicted growth and this job's predicted peak come to 1.0 GiB, above the memory budget of 1.0 GiB",
        ),
    );
}

#[test]
fn s20_unknown_memory_estimates_gate_nothing() {
    let s = WorkSettings {
        memory_budget: MemBudget::Kib(1000),
        ..settings()
    };
    let known_used = machine_with(Signals {
        memory_used_kib: Some(900),
        ..calm()
    });
    let running_without_p90 = run("r", 1.0, Some(1.0));
    let cand = wait("c", 1.0, Some(1.0), 10.0);
    assert_eq!(
        admit_now(
            &cand,
            std::slice::from_ref(&running_without_p90),
            &s,
            &known_used
        ),
        None
    );
    let unknown_used = machine_with(Signals {
        memory_used_kib: None,
        ..calm()
    });
    let cand_with_p90 = WaitingJob {
        pred_rss_p90_kib: Some(500),
        ..wait("c", 1.0, Some(1.0), 10.0)
    };
    assert_eq!(admit_now(&cand_with_p90, &[], &s, &unknown_used), None);
}

#[test]
fn s21_used_memory_over_budget_is_memory_pressure_not_a_reservation() {
    let s = WorkSettings {
        memory_budget: MemBudget::Kib(1_000_000),
        ..settings()
    };
    let m = machine_with(Signals {
        memory_used_kib: Some(1_100_000),
        ..calm()
    });
    let running = RunningJob {
        rss_kib: Some(0),
        ..run("r", 1.0, Some(1.0))
    };
    assert_eq!(
        admit_now(
            &wait("c", 1.0, Some(1.0), 10.0),
            std::slice::from_ref(&running),
            &s,
            &m
        ),
        hold(
            Cause::Memory,
            "used memory is 1.0 GiB, above the memory budget of 1.0 GiB"
        ),
    );
}

#[test]
fn s22_idle_holds_a_tiny_job_too() {
    let busy = [run("r", 7.9, None)];
    let tiny = wait("t", 1.0, Some(0.2), 10.0);
    let m = machine_with(Signals {
        idle_pct: Some(5.0),
        ..calm()
    });
    let s = WorkSettings {
        idle_min: 10.0,
        ..settings()
    };
    assert_eq!(
        admit_now(&tiny, &busy, &s, &m),
        hold(
            Cause::Idle,
            "the CPU is 5% idle; jobs start at 10% idle or more"
        ),
    );
}
