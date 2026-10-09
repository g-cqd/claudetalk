use super::*;

// ---- R5: machine floors over ten-minute buckets ---------------------------------------------------------

#[test]
fn disk_below_its_floor_pauses_admission_and_opens_a_blocker() {
    let mut store = Store::new(ReconcileInput {
        machine: Some(disk(DISK_BREACH_KIB)),
        ..input()
    });
    assert_breach_pass(
        &store.pass(at(0)),
        BUCKET_10_MIN,
        "disk free 3.1 GiB below the floor of 5.0 GiB",
    );
}

#[test]
fn a_breach_repeated_within_the_same_ten_minute_bucket_adds_nothing() {
    let mut store = Store::new(ReconcileInput {
        machine: Some(disk(DISK_BREACH_KIB)),
        ..input()
    });
    store.pass(at(0));
    assert!(store.pass(at(5)).is_empty());
}

#[test]
fn a_breach_still_standing_in_the_next_bucket_refreshes_only_the_blocker() {
    let mut store = Store::new(ReconcileInput {
        machine: Some(disk(DISK_BREACH_KIB)),
        ..input()
    });
    store.pass(at(0));
    let actions = store.pass(at(10));
    assert!(
        matches!(
            actions.as_slice(),
            [ReconcileAction::Blocker { key, open: true, .. }]
                if key == &format!("blocker:{}", BUCKET_10_MIN + 1)
        ),
        "admission stays paused; only the blocker is refreshed, got {actions:?}"
    );
}

#[test]
fn recovery_resumes_admission_and_closes_the_blocker_once() {
    let mut store = Store::new(ReconcileInput {
        machine: Some(disk(DISK_BREACH_KIB)),
        ..input()
    });
    store.pass(at(0));
    store.input.machine = Some(disk(HEALTHY_KIB));
    let actions = store.pass(at(10));
    assert!(
        matches!(
            actions.as_slice(),
            [
                ReconcileAction::Admission { key, paused: None },
                ReconcileAction::Blocker { key: blocker, open: false, .. },
            ] if key == &format!("resume:{}", BUCKET_10_MIN + 1) && blocker == "unblock:blocker-1"
        ),
        "got {actions:?}"
    );
    assert!(store.pass(at(20)).is_empty(), "the close is not repeated");
}

#[test]
fn memory_floor_breach_names_the_available_memory_in_gib() {
    let i = ReconcileInput {
        machine: Some(MachineSnapshot {
            cpus: 8.0,
            total_memory_kib: Some(16 * GIB_KIB),
            disk_free_kib: Some(HEALTHY_KIB),
            signals: Some(Signals {
                memory_used_kib: Some(15 * GIB_KIB + GIB_KIB / 2),
                ..Signals::default()
            }),
        }),
        settings: WorkSettings {
            memory_floor_kib: Some(2 * GIB_KIB),
            ..WorkSettings::default()
        },
        ..input()
    };
    assert_breach_pass(
        &plan(&i),
        BUCKET_10_MIN,
        "available memory 0.5 GiB below the floor of 2.0 GiB",
    );
}

#[test]
fn cpu_idle_floor_breach_names_the_idle_percentage() {
    let i = ReconcileInput {
        machine: Some(MachineSnapshot {
            cpus: 8.0,
            total_memory_kib: None,
            disk_free_kib: Some(HEALTHY_KIB),
            signals: Some(Signals {
                idle_pct: Some(4.4),
                ..Signals::default()
            }),
        }),
        settings: WorkSettings {
            idle_floor_pct: Some(10.0),
            ..WorkSettings::default()
        },
        ..input()
    };
    assert_breach_pass(
        &plan(&i),
        BUCKET_10_MIN,
        "CPU 4% idle below the floor of 10%",
    );
}
