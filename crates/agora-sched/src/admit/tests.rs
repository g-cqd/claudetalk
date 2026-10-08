use super::*;

const NOW: f64 = 10_000.0;

fn fixture() -> Budgets {
    Budgets {
        cpus: 8.0,
        budget: 8.0,
        capacity: None,
        memory_budget_kib: None,
        load_limit: 12.0,
    }
}

fn calm() -> Signals {
    Signals {
        idle_pct: Some(90.0),
        memory_level: Some(MemLevel::Normal),
        load1: None,
        load_gate: false,
        memory_used_kib: None,
    }
}

fn run(id: &str, weight: f64, pred: Option<f64>) -> RunningJob {
    RunningJob {
        id: id.to_owned(),
        weight,
        pred_cores: pred,
        pred_rss_p90_kib: None,
        pred_duration_p90_s: None,
        rss_kib: None,
        started_s: Some(NOW),
        quiet_until_s: None,
    }
}

fn job(id: &str, weight: f64, pred: Option<f64>, age_s: f64) -> WaitingJob {
    WaitingJob {
        id: id.to_owned(),
        kind: "work".to_owned(),
        seq: 0,
        requested_s: NOW - age_s,
        weight,
        pred_cores: pred,
        pred_duration_s: None,
        pred_duration_p90_s: None,
        pred_rss_p90_kib: None,
        machine_held_since_s: None,
    }
}

fn queued(id: &str, kind: &str, seq: i64, requested_s: f64, dur: Option<f64>) -> WaitingJob {
    WaitingJob {
        id: id.to_owned(),
        kind: kind.to_owned(),
        seq,
        requested_s,
        weight: 1.0,
        pred_cores: None,
        pred_duration_s: dur,
        pred_duration_p90_s: None,
        pred_rss_p90_kib: None,
        machine_held_since_s: None,
    }
}

fn admit(
    job: &WaitingJob,
    running: &[RunningJob],
    s: &WorkSettings,
    b: &Budgets,
    sig: &Signals,
) -> Option<Hold> {
    admission(job, running, s, b, Some(sig), NOW)
}

fn text(hold: Option<Hold>) -> String {
    hold.map_or_else(|| "none".to_owned(), |h| h.reason)
}

fn order(mut jobs: Vec<WaitingJob>, aging: f64) -> Vec<String> {
    jobs.sort_by(|a, b| waiting_order_cmp(a, b, aging));
    jobs.into_iter().map(|j| j.id).collect()
}

#[test]
fn cores_fit_while_the_sum_stays_within_the_budget() {
    let b = fixture();
    let s = WorkSettings::default();
    let four: Vec<RunningJob> = (0..4)
        .map(|i| run(&format!("r{i}"), 2.0, Some(1.0)))
        .collect();
    assert!(admit(&job("c", 2.0, Some(1.0), 10.0), &four, &s, &b, &calm()).is_none());
    assert!(admit(&job("c", 6.0, Some(9.0), 10.0), &[], &s, &b, &calm()).is_none());
}

#[test]
fn cores_hold_names_the_taken_and_needed_cores() {
    let b = fixture();
    let s = WorkSettings::default();
    let mut running: Vec<RunningJob> = (0..4)
        .map(|i| run(&format!("r{i}"), 2.0, Some(1.0)))
        .collect();
    running.push(run("big", 4.0, Some(3.4)));
    assert_eq!(
        text(admit(&job("c", 2.0, None, 10.0), &running, &s, &b, &calm())),
        "predicted 7.4 of 8 cores are taken and this job needs 2"
    );
}

#[test]
fn tiny_jobs_pass_the_budget_only_up_to_half_a_core() {
    let b = fixture();
    let s = WorkSettings::default();
    let busy = [run("busy", 7.9, None)];
    assert!(admit(&job("t", 1.0, Some(0.2), 10.0), &busy, &s, &b, &calm()).is_none());
    assert!(admit(&job("t", 1.0, Some(0.5), 10.0), &busy, &s, &b, &calm()).is_none());
    assert_eq!(
        text(admit(
            &job("t", 1.0, Some(0.6), 10.0),
            &busy,
            &s,
            &b,
            &calm()
        )),
        "predicted 7.9 of 8 cores are taken and this job needs 0.6"
    );
    assert_eq!(
        text(admit(
            &job("t", 1.0, Some(0.5000000011), 10.0),
            &busy,
            &s,
            &b,
            &calm()
        )),
        "predicted 7.9 of 8 cores are taken and this job needs 0.5"
    );
}

#[test]
fn the_budget_tolerates_one_nanocore() {
    let b = fixture();
    let s = WorkSettings::default();
    let busy = [run("busy", 7.0, None)];
    assert!(admit(&job("c", 1.0, None, 10.0), &busy, &s, &b, &calm()).is_none());
    assert_eq!(
        text(admit(
            &job("c", 2.0, Some(1.0000000011), 10.0),
            &busy,
            &s,
            &b,
            &calm()
        )),
        "predicted 7 of 8 cores are taken and this job needs 1"
    );
}

#[test]
fn an_exclusive_job_gets_no_tiny_bypass() {
    let b = fixture();
    let s = WorkSettings::default();
    let busy = [run("busy", 8.0, Some(7.9))];
    assert_eq!(
        text(admit(
            &job("x", 8.0, Some(0.2), 10.0),
            &busy,
            &s,
            &b,
            &calm()
        )),
        "predicted 7.9 of 8 cores are taken and this job needs 0.2"
    );
}

#[test]
fn capacity_caps_declared_weights_and_is_skipped_when_nothing_runs() {
    let b = Budgets {
        capacity: Some(4.0),
        ..fixture()
    };
    let s = WorkSettings::default();
    let busy = [run("busy", 3.0, Some(0.5))];
    let expected =
        "declared weights of 3 of the capacity of 4 cores are taken and this job declares 2";
    assert_eq!(
        text(admit(
            &job("c", 2.0, Some(0.5), 10.0),
            &busy,
            &s,
            &b,
            &calm()
        )),
        expected
    );
    assert_eq!(
        text(admit(
            &job("c", 2.0, Some(0.3), 10.0),
            &busy,
            &s,
            &b,
            &calm()
        )),
        expected
    );
    assert!(
        admit(
            &job("c", 2.0, Some(0.5), 10.0),
            &busy,
            &s,
            &fixture(),
            &calm()
        )
        .is_none()
    );
    let tight = Budgets {
        capacity: Some(1.0),
        ..fixture()
    };
    assert!(admit(&job("c", 8.0, None, 10.0), &[], &s, &tight, &calm()).is_none());
}

#[test]
fn memory_limit_setting_gates_memory_pressure() {
    let b = fixture();
    let warn = Signals {
        memory_level: Some(MemLevel::Warning),
        ..calm()
    };
    let critical = Signals {
        memory_level: Some(MemLevel::Critical),
        ..calm()
    };
    let with = |limit: MemLimit| WorkSettings {
        memory_limit: limit,
        ..WorkSettings::default()
    };
    assert_eq!(
        text(pressure(&with(MemLimit::Warning), &b, Some(&warn))),
        "memory pressure is warning; jobs start below warning"
    );
    assert!(pressure(&with(MemLimit::Critical), &b, Some(&warn)).is_none());
    assert_eq!(
        text(pressure(&with(MemLimit::Critical), &b, Some(&critical))),
        "memory pressure is critical; jobs start below critical"
    );
    assert!(pressure(&with(MemLimit::Off), &b, Some(&critical)).is_none());
    assert!(pressure(&with(MemLimit::Warning), &b, None).is_none());
}

#[test]
fn used_memory_above_the_budget_is_pressure_and_equal_is_not() {
    let b = Budgets {
        memory_budget_kib: Some(1_000_000),
        ..fixture()
    };
    let s = WorkSettings::default();
    let over_budget = Signals {
        memory_used_kib: Some(1_100_000),
        ..calm()
    };
    let at_budget = Signals {
        memory_used_kib: Some(1_000_000),
        ..calm()
    };
    assert_eq!(
        text(pressure(&s, &b, Some(&over_budget))),
        "used memory is 1.0 GiB, above the memory budget of 1.0 GiB"
    );
    assert!(pressure(&s, &b, Some(&at_budget)).is_none());
}

#[test]
fn idle_gate_rounds_the_percent_in_its_text() {
    let b = fixture();
    let s = WorkSettings {
        idle_min: 15.0,
        ..WorkSettings::default()
    };
    let idle = |pct: Option<f64>| Signals {
        idle_pct: pct,
        ..calm()
    };
    assert_eq!(
        text(pressure(&s, &b, Some(&idle(Some(8.0))))),
        "the CPU is 8% idle; jobs start at 15% idle or more"
    );
    assert!(pressure(&s, &b, Some(&idle(Some(15.0)))).is_none());
    assert_eq!(
        text(pressure(&s, &b, Some(&idle(Some(14.6))))),
        "the CPU is 15% idle; jobs start at 15% idle or more"
    );
    assert!(pressure(&s, &b, Some(&idle(None))).is_none());
}

#[test]
fn load_gate_holds_at_the_limit_only_when_the_gate_is_on() {
    let b = fixture();
    let s = WorkSettings::default();
    let load = |value: f64, gate: bool| Signals {
        load1: Some(value),
        load_gate: gate,
        ..calm()
    };
    assert_eq!(
        text(pressure(&s, &b, Some(&load(92.8, true)))),
        "the load average 92.80 is at or above 12.00"
    );
    assert!(pressure(&s, &b, Some(&load(12.0, true))).is_some());
    assert!(pressure(&s, &b, Some(&load(11.99, true))).is_none());
    assert!(pressure(&s, &b, Some(&load(92.8, false))).is_none());
}

#[test]
fn pressure_names_the_first_signal_in_its_order() {
    let b = Budgets {
        memory_budget_kib: Some(1_000_000),
        ..fixture()
    };
    let s = WorkSettings::default();
    let busy_machine = |level: MemLevel| Signals {
        memory_level: Some(level),
        memory_used_kib: Some(1_100_000),
        idle_pct: Some(5.0),
        load1: Some(20.0),
        load_gate: true,
    };
    assert_eq!(
        text(pressure(&s, &b, Some(&busy_machine(MemLevel::Warning)))),
        "memory pressure is warning; jobs start below warning"
    );
    let no_level = Signals {
        memory_level: Some(MemLevel::Normal),
        ..busy_machine(MemLevel::Normal)
    };
    assert!(text(pressure(&s, &b, Some(&no_level))).starts_with("used memory is"));
    let within_budget = Signals {
        memory_used_kib: Some(900_000),
        ..no_level
    };
    assert!(text(pressure(&s, &b, Some(&within_budget))).starts_with("the CPU is 5% idle"));
    let idle_ok = Signals {
        idle_pct: Some(90.0),
        ..within_budget
    };
    assert!(text(pressure(&s, &b, Some(&idle_ok))).starts_with("the load average 20.00"));
    let cores_first = admit(
        &job("c", 1.0, None, 10.0),
        &[run("r", 8.0, None)],
        &s,
        &Budgets {
            capacity: Some(4.0),
            ..b.clone()
        },
        &busy_machine(MemLevel::Warning),
    );
    assert_eq!(cause(&cores_first), Some(Cause::Cores));
}

fn cause(hold: &Option<Hold>) -> Option<Cause> {
    hold.as_ref().map(|h| h.cause)
}

mod backfill;
mod reserve;
