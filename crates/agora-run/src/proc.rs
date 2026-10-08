//! Process probes and signalling (via rustix and sysinfo). The one unsafe module in this crate is supervise/rusage.rs.

use std::collections::{HashMap, HashSet};
use std::io;

use rustix::process::{Pid as OsPid, Signal, kill_process, kill_process_group};
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

/// Tolerance, in seconds, between a recorded process start time and the live one (D8 pid-reuse guard).
const START_TOLERANCE_S: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sig {
    Term,
    Kill,
    Int,
    Hup,
}

/// Refreshes exactly one process; a process that no longer exists is removed from the table. Start time, status and
/// parent are base facts that sysinfo always fills, so no optional refresh kind is needed.
fn refresh_one(sys: &mut System, pid: Pid) {
    let _ = sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
}

/// process start time (seconds since epoch) via sysinfo; None if absent.
pub fn start_time_s(pid: u32) -> Option<u64> {
    let pid = Pid::from_u32(pid);
    let mut sys = System::new();
    refresh_one(&mut sys, pid);
    sys.process(pid).map(|process| process.start_time())
}

/// pid exists, is not a zombie, and (if `start_s` is Some) its start time is within +-2 s of it.
/// Pid 0 (the kernel's own group, never a job) is never alive.
pub fn alive(pid: u32, start_s: Option<u64>) -> bool {
    if pid == 0 {
        return false;
    }
    let pid = Pid::from_u32(pid);
    let mut sys = System::new();
    refresh_one(&mut sys, pid);
    let Some(process) = sys.process(pid) else {
        return false;
    };
    if process.status() == ProcessStatus::Zombie {
        return false;
    }
    start_s.is_none_or(|expected| process.start_time().abs_diff(expected) <= START_TOLERANCE_S)
}

/// Maps a pid to rustix's non-zero pid type. Zero and values above `i32::MAX` are rejected, so a bad pid can never
/// reach `kill(0, ..)`, which names our own group.
fn os_pid(pid: u32) -> io::Result<OsPid> {
    i32::try_from(pid)
        .ok()
        .and_then(OsPid::from_raw)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid process id {pid}"),
            )
        })
}

/// Maps a process-group id to rustix's pid type. Groups 0 and 1 are refused: `kill(-0, ..)` names our own group and
/// `kill(-1, ..)` names every process we may signal.
fn os_group(pgid: u32) -> io::Result<OsPid> {
    if pgid < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid process group {pgid}"),
        ));
    }
    os_pid(pgid)
}

fn os_signal(sig: Sig) -> Signal {
    match sig {
        Sig::Term => Signal::TERM,
        Sig::Kill => Signal::KILL,
        Sig::Int => Signal::INT,
        Sig::Hup => Signal::HUP,
    }
}

pub fn signal_pid(pid: u32, sig: Sig) -> io::Result<()> {
    kill_process(os_pid(pid)?, os_signal(sig)).map_err(io::Error::from)
}

pub fn signal_group(pgid: u32, sig: Sig) -> io::Result<()> {
    kill_process_group(os_group(pgid)?, os_signal(sig)).map_err(io::Error::from)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TreeSample {
    pub cpu_percent: f64,
    pub rss_kib: u64,
}

pub struct Sampler {
    sys: sysinfo::System,
}

impl Sampler {
    pub fn new() -> Sampler {
        Sampler { sys: System::new() }
    }

    /// Sums cpu% and RSS over `root` and its descendants (by parent pid, transitively). sysinfo does not expose a
    /// process's process-group id (`group_id` is the real gid), so processes that share the group without being
    /// descendants (for example a child that called setsid) are not counted. Returns None if `root` does not exist.
    /// The first call may report 0% cpu; cpu% is a delta, so keep one `Sampler` for a process's lifetime.
    pub fn sample_tree(&mut self, root: u32) -> Option<TreeSample> {
        let root = Pid::from_u32(root);
        let _ = self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        self.sys.process(root)?;

        let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
        for (pid, process) in self.sys.processes() {
            if let Some(parent) = process.parent() {
                children.entry(parent).or_default().push(*pid);
            }
        }

        let mut seen = HashSet::new();
        let mut stack = vec![root];
        let mut cpu_percent = 0.0_f64;
        let mut rss_bytes = 0_u64;
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            if let Some(process) = self.sys.process(pid) {
                cpu_percent += f64::from(process.cpu_usage());
                rss_bytes = rss_bytes.saturating_add(process.memory());
            }
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids.iter().copied());
            }
        }
        Some(TreeSample {
            cpu_percent,
            rss_kib: rss_bytes / 1024,
        })
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Sampler::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::process::test_kill_process;
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    /// Kills and reaps its child on drop, so no test leaks a process. When the flag is set the child leads its own
    /// process group, and that whole group is killed first, so a background member does not outlive a failing test.
    struct Guard(Child, bool);

    impl Guard {
        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if self.1 {
                // The child is its group's leader, so its pid is the pgid and never names our own group.
                let _ = signal_group(self.pid(), Sig::Kill);
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn sleeper() -> Guard {
        Guard(
            Command::new("sleep").arg("5").spawn().expect("spawn sleep"),
            false,
        )
    }

    /// Polls `try_wait` for at most 3 s; None if the child is still running.
    fn wait_exit(child: &mut Child) -> Option<ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Polls `cond` for at most 3 s; false if it never held.
    fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !cond() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    /// The pid of a child that has been spawned, run to completion and reaped: no live process has it now.
    fn reaped_pid() -> u32 {
        let mut child = Command::new("true").spawn().expect("spawn true");
        let pid = child.id();
        assert!(wait_exit(&mut child).is_some(), "true exits");
        pid
    }

    /// `sh -c 'sleep 5 & echo $!; sleep 5'`, returning the guarded shell and the pid of its background sleep. The pid
    /// is read from the shell's stdout, so the background child exists before the test goes on.
    fn shell_with_background(group_leader: bool) -> (Guard, u32) {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 5 & echo $!; sleep 5"])
            .stdout(Stdio::piped());
        if group_leader {
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().expect("spawn sh");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("read background pid");
        let background = line.trim().parse().expect("background pid");
        (Guard(child, group_leader), background)
    }

    fn now_s() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs()
    }

    #[test]
    fn start_time_is_within_five_seconds_of_now() {
        let child = sleeper();
        let start = start_time_s(child.pid()).expect("start time of a live child");
        assert!(
            start.abs_diff(now_s()) <= 5,
            "start {start} is not near now"
        );
    }

    #[test]
    fn start_time_is_none_for_a_reaped_pid() {
        assert_eq!(start_time_s(reaped_pid()), None);
    }

    #[test]
    fn alive_is_true_for_a_running_child_with_or_without_its_start_time() {
        let child = sleeper();
        let start = start_time_s(child.pid()).expect("start time");
        assert!(alive(child.pid(), None));
        assert!(alive(child.pid(), Some(start)));
    }

    #[test]
    fn alive_tolerates_two_seconds_of_start_time_drift_and_no_more() {
        let child = sleeper();
        let start = start_time_s(child.pid()).expect("start time");
        assert!(alive(child.pid(), Some(start + 2)));
        assert!(alive(child.pid(), Some(start - 2)));
        assert!(!alive(child.pid(), Some(start + 3)));
        assert!(!alive(child.pid(), Some(start - 3)));
    }

    #[test]
    fn alive_is_false_with_a_wrong_start_time() {
        let child = sleeper();
        let start = start_time_s(child.pid()).expect("start time");
        assert!(!alive(child.pid(), Some(start - 100)));
    }

    #[test]
    fn alive_is_false_after_kill_and_wait() {
        let mut guard = sleeper();
        let pid = guard.pid();
        guard.0.kill().expect("kill the child");
        assert!(
            wait_exit(&mut guard.0).is_some(),
            "child exits after SIGKILL"
        );
        assert!(!alive(pid, None));
    }

    #[test]
    fn alive_is_false_for_a_killed_child_that_is_not_yet_reaped() {
        let mut guard = sleeper();
        let pid = guard.pid();
        guard.0.kill().expect("kill the child");
        assert!(wait_until(|| !alive(pid, None)), "zombie is not alive");
        // The kernel still holds the zombie (kill(pid, 0) succeeds until wait() reaps it), so the
        // zombie check, not a missing process, is what made `alive` false.
        let os = os_pid(pid).expect("a real pid");
        assert!(test_kill_process(os).is_ok(), "zombie still exists");
    }

    #[test]
    fn a_reaped_pid_is_not_alive_and_samples_to_none() {
        let pid = reaped_pid();
        assert!(!alive(pid, None));
        assert!(!alive(pid, Some(1)));
        assert_eq!(Sampler::new().sample_tree(pid), None);
    }

    #[test]
    fn pid_zero_is_never_alive() {
        assert!(!alive(0, None));
    }

    #[test]
    fn signal_pid_kill_ends_a_child_by_signal() {
        let mut guard = sleeper();
        signal_pid(guard.pid(), Sig::Kill).expect("signal a live child");
        let status = wait_exit(&mut guard.0).expect("child exits after SIGKILL");
        assert_eq!(status.signal(), Some(9));
    }

    #[test]
    fn signal_group_ends_the_leader_and_its_background_member() {
        let (mut guard, background) = shell_with_background(true);
        assert!(
            alive(background, None),
            "background sleep runs before the signal"
        );
        signal_group(guard.pid(), Sig::Term).expect("signal the group");
        assert!(
            wait_exit(&mut guard.0).is_some(),
            "group leader exits within 3 s"
        );
        assert!(
            wait_until(|| !alive(background, None)),
            "background member of the group gets the signal"
        );
    }

    #[test]
    fn pid_zero_is_rejected_by_both_signal_functions() {
        let pid_zero = signal_pid(0, Sig::Term).expect_err("pid 0 is not a target");
        assert_eq!(pid_zero.kind(), io::ErrorKind::InvalidInput);
        let group_zero = signal_group(0, Sig::Kill).expect_err("group 0 is not a target");
        assert_eq!(group_zero.kind(), io::ErrorKind::InvalidInput);
        assert!(signal_group(1, Sig::Term).is_err());
    }

    #[test]
    fn sig_maps_to_the_matching_os_signal() {
        assert_eq!(os_signal(Sig::Term), Signal::TERM);
        assert_eq!(os_signal(Sig::Kill), Signal::KILL);
        assert_eq!(os_signal(Sig::Int), Signal::INT);
        assert_eq!(os_signal(Sig::Hup), Signal::HUP);
    }

    #[test]
    fn sample_tree_of_a_child_has_resident_memory_and_finite_cpu() {
        let child = sleeper();
        let sample = Sampler::new()
            .sample_tree(child.pid())
            .expect("a live child is sampled");
        assert!(sample.rss_kib > 0, "a running sleep has resident memory");
        assert!(sample.cpu_percent.is_finite() && sample.cpu_percent >= 0.0);
    }

    #[test]
    fn sample_tree_counts_descendants_on_top_of_the_root() {
        let (mut guard, background) = shell_with_background(false);
        let single = Sampler::new()
            .sample_tree(background)
            .expect("background sleep is sampled");
        let tree = Sampler::new()
            .sample_tree(guard.pid())
            .expect("shell is sampled");
        assert!(
            tree.rss_kib > single.rss_kib,
            "shell plus its sleeps ({} KiB) exceeds one sleep ({} KiB)",
            tree.rss_kib,
            single.rss_kib
        );
        let _ = signal_pid(background, Sig::Kill);
        let _ = guard.0.kill();
        let _ = guard.0.wait();
    }
}
