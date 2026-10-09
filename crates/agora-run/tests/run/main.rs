//! Integration tests for agora-run through its public API: program lookup, QoS wrapping, job directories, log tails,
//! process probes and machine probes. Every child process runs under a guard that kills it on drop, and every wait is
//! bounded by `WAIT`.

mod jobdir;
mod locate;
mod machine;
mod proc;
mod qos;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::num::NonZeroUsize;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agora_run::RunError;
use agora_run::jobdir::{
    JobDir, JobSpec, JobStatus, Sample, SupervisorInfo, create_log, log_path, tail, work_root,
};
use agora_run::locate::locate;
use agora_run::machine::{FakeMachine, Machine, RealMachine, effective_cpus};
use agora_run::proc::{Sampler, Sig, alive, signal_group, signal_pid, start_time_s};
use agora_run::qos::wrap;
use agora_sched::admit::{MachineSnapshot, MemLevel, Signals};
use agora_sched::settings::Qos;

/// Longest any single wait may block before the test fails.
const WAIT: Duration = Duration::from_secs(5);
/// Interval between probes while waiting.
const STEP: Duration = Duration::from_millis(10);
const MIB: usize = 1024 * 1024;

/// A child that is killed and reaped on drop. A group leader also has its process group killed, so a background
/// member it started does not outlive a failing test.
struct Guard {
    child: Child,
    group: bool,
}

impl Guard {
    fn spawn(cmd: &mut Command) -> Guard {
        Guard {
            child: cmd.spawn().expect("spawn child"),
            group: false,
        }
    }

    fn group_leader(cmd: &mut Command) -> Guard {
        cmd.process_group(0);
        Guard {
            child: cmd.spawn().expect("spawn group leader"),
            group: true,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.group {
            let _ = signal_group(self.pid(), Sig::Kill);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Polls `probe` every `STEP` until it yields a value; None once `WAIT` has passed.
fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(STEP);
    }
}

fn wait_exit(child: &mut Child) -> Option<ExitStatus> {
    wait_for(|| child.try_wait().ok().flatten())
}

fn holds(mut cond: impl FnMut() -> bool) -> bool {
    wait_for(|| cond().then_some(())).is_some()
}

/// A `sleep 30` child, killed on drop.
fn sleeper() -> Guard {
    Guard::spawn(Command::new("sleep").arg("30"))
}

/// `sh -c 'sleep 30 & echo $!; wait'` as a group leader. Returns the guard and the pid of the background sleep,
/// which the script echoes first, so the background member exists before the caller goes on.
fn group_with_background_sleep() -> (Guard, u32) {
    let mut guard = Guard::group_leader(
        Command::new("sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped()),
    );
    let stdout = guard.child.stdout.take().expect("piped stdout");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("read the background pid");
    let background: u32 = line.trim().parse().expect("background pid");
    (guard, background)
}

/// The pid of a child that has run to completion and been reaped.
fn reaped_pid() -> u32 {
    let mut child = Command::new("true").spawn().expect("spawn true");
    let pid = child.id();
    assert!(wait_exit(&mut child).is_some_and(|status| status.success()));
    pid
}
