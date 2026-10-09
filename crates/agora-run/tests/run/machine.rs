use super::*;

// ---------------------------------------------------------------------------------------------------------------------
// machine: the fake double and the real probes

#[test]
fn fake_machine_reports_its_documented_defaults() {
    let snapshot = FakeMachine::new().snapshot();
    assert_eq!(
        snapshot,
        MachineSnapshot {
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
        }
    );
}

#[test]
fn fake_machine_set_replaces_every_field_of_the_snapshot() {
    let machine = FakeMachine::new();
    let custom = MachineSnapshot {
        cpus: 2.0,
        total_memory_kib: None,
        disk_free_kib: None,
        signals: None,
    };
    machine.set(custom.clone());
    assert_eq!(machine.snapshot(), custom);
}

#[test]
fn fake_machine_update_changes_only_the_field_it_names() {
    let machine = FakeMachine::new();
    machine.update(|snap| snap.cpus = 2.0);
    let mut expected = FakeMachine::new().snapshot();
    expected.cpus = 2.0;
    assert_eq!(machine.snapshot(), expected);
}

#[test]
fn fake_machine_update_of_signals_is_visible_to_the_next_snapshot() {
    let machine = FakeMachine::new();
    machine.update(|snap| {
        if let Some(signals) = snap.signals.as_mut() {
            signals.memory_level = Some(MemLevel::Critical);
            signals.load1 = Some(3.5);
        }
    });
    let signals = machine.snapshot().signals.expect("signals stay set");
    assert_eq!(signals.memory_level, Some(MemLevel::Critical));
    assert_eq!(signals.load1, Some(3.5));
    assert_eq!(
        signals.idle_pct,
        Some(90.0),
        "untouched fields keep their value"
    );
}

#[test]
fn real_machine_has_no_idle_reading_until_a_baseline_exists() {
    let home = tempfile::tempdir().expect("tempdir");
    let machine = RealMachine::new(home.path().to_path_buf());
    let first = machine.snapshot().signals.expect("signals");
    assert_eq!(first.idle_pct, None);
    // sysinfo measures CPU usage only across a span of at least MINIMUM_CPU_UPDATE_INTERVAL (200 ms).
    std::thread::sleep(Duration::from_millis(250));
    let second = machine.snapshot().signals.expect("signals");
    let idle = second.idle_pct.expect("the second snapshot reports idle");
    assert!((0.0..=100.0).contains(&idle), "idle {idle}");
}

#[test]
fn real_machine_reports_free_disk_and_memory_figures_for_its_home() {
    let home = tempfile::tempdir().expect("tempdir");
    let snapshot = RealMachine::new(home.path().to_path_buf()).snapshot();
    assert!(snapshot.disk_free_kib.is_some_and(|kib| kib > 0));
    assert!(snapshot.total_memory_kib.is_some_and(|kib| kib > 0));
    let signals = snapshot.signals.expect("signals");
    assert!(signals.memory_used_kib.is_some_and(|kib| kib > 0));
}

#[test]
fn real_machine_reports_a_positive_cpu_count() {
    let home = tempfile::tempdir().expect("tempdir");
    let snapshot = RealMachine::new(home.path().to_path_buf()).snapshot();
    assert!(snapshot.cpus > 0.0, "cpus {}", snapshot.cpus);
}

#[test]
fn real_machine_has_no_disk_figure_for_a_home_that_does_not_exist() {
    let home = tempfile::tempdir().expect("tempdir");
    let missing = RealMachine::new(home.path().join("no-such-home")).snapshot();
    assert_eq!(missing.disk_free_kib, None);
}

#[test]
fn real_machine_enables_the_load_gate_only_on_linux() {
    let home = tempfile::tempdir().expect("tempdir");
    let signals = RealMachine::new(home.path().to_path_buf())
        .snapshot()
        .signals
        .expect("signals");
    assert_eq!(signals.load_gate, cfg!(target_os = "linux"));
}

#[test]
fn effective_cpus_is_positive_and_no_more_than_the_host_parallelism() {
    // A fractional cgroup quota can bring the count below one, so only positivity is certain.
    let host = std::thread::available_parallelism().map_or(1, NonZeroUsize::get) as f64;
    let cpus = effective_cpus();
    assert!(cpus > 0.0 && cpus <= host, "{cpus} of a host with {host}");
}
