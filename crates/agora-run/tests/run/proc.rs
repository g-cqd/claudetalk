use super::*;

// ---------------------------------------------------------------------------------------------------------------------
// proc: liveness, signalling, sampling

#[test]
fn alive_is_true_for_a_running_child_with_or_without_its_start_time() {
    let child = sleeper();
    let start = start_time_s(child.pid()).expect("a running child has a start time");
    assert!(alive(child.pid(), None));
    assert!(alive(child.pid(), Some(start)));
}

#[test]
fn alive_is_false_when_the_recorded_start_time_does_not_match() {
    let child = sleeper();
    let start = start_time_s(child.pid()).expect("start time");
    assert!(!alive(child.pid(), Some(start - 100)));
}

#[test]
fn alive_is_false_once_the_child_is_killed_and_reaped() {
    let mut child = sleeper();
    let pid = child.pid();
    let start = start_time_s(pid).expect("start time");
    child.child.kill().expect("kill the child");
    assert!(wait_exit(&mut child.child).is_some(), "the child is reaped");
    assert!(!alive(pid, None));
    assert!(!alive(pid, Some(start)));
}

#[test]
fn signal_to_a_reaped_pid_fails_with_no_such_process() {
    let err = signal_pid(reaped_pid(), Sig::Term).expect_err("no process has that pid");
    assert_eq!(
        err.raw_os_error(),
        Some(rustix::io::Errno::SRCH.raw_os_error())
    );
}

#[test]
fn signal_group_ends_the_leader_and_the_background_member_it_started() {
    let (mut leader, background) = group_with_background_sleep();
    assert!(alive(background, None), "background sleep runs first");
    signal_group(leader.pid(), Sig::Term).expect("signal the group");
    let status = wait_exit(&mut leader.child).expect("group leader exits");
    assert_eq!(
        status.signal(),
        Some(rustix::process::Signal::TERM.as_raw())
    );
    assert!(
        holds(|| !alive(background, None)),
        "the background member gets the group signal"
    );
}

#[test]
fn sampler_reports_resident_memory_for_a_busy_child() {
    let busy = Guard::spawn(Command::new("sh").args(["-c", "while :; do :; done"]));
    let sample = Sampler::new()
        .sample_tree(busy.pid())
        .expect("a live child is sampled");
    assert!(sample.rss_kib > 0, "rss {} KiB", sample.rss_kib);
}

#[test]
fn sampler_reports_cpu_for_a_busy_child_once_two_samples_span_an_interval() {
    let busy = Guard::spawn(Command::new("sh").args(["-c", "while :; do :; done"]));
    let mut sampler = Sampler::new();
    let cpu = wait_for(|| {
        sampler
            .sample_tree(busy.pid())
            .filter(|sample| sample.cpu_percent > 0.0)
    });
    assert!(
        cpu.is_some(),
        "a busy child reports cpu usage within {WAIT:?}"
    );
}
