//! The per-job supervisor (D8): runs in the `agora _supervise --job-dir D` / `agora-supervise --job-dir D` process.
//!
//! Lifecycle: detach (setsid, signal flags), read the spec, open the log, launch the command in its own process
//! group, publish supervisor.json, then poll it for its exit: every 25 ms while the job runs normally, and every poll
//! enforces the log cap (TERM once it is passed). Once TERM has been sent, for the cap or a cancel, the poll is every
//! 1 ms until the child exits, and KILL comes at once if the log then grows 16 MiB beyond the cap. A 500 ms tick,
//! which is not tied to the poll, honours the cancel file and TERM/INT/HUP, KILLs after the grace period, and samples
//! the process tree on every fourth tick. After the reap the
//! CPU time and peak RSS come from getrusage (see `rusage`), and status.json is recorded. Any failure before that is
//! recorded as "could not supervise" in status.json.

mod rusage;

use std::fs::File;
use std::io::{self, ErrorKind, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agora_sched::exit::{exit_status, spawn_error_status};
use agora_sched::textfmt::brief;
use rustix::fs::{Access, FileType, Mode, OFlags, access, fstat, open};
use rustix::io::Errno;
use rustix::process::setsid;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

use crate::RunError;
use crate::jobdir::{JobDir, JobSpec, JobStatus, Sample, SupervisorInfo};
use crate::locate::locate;
use crate::machine::{Machine, RealMachine};
use crate::proc::{Sampler, Sig, signal_group, start_time_s};
use crate::qos;
use rusage::children;

/// How often a running child is polled for its exit and its log is checked against the cap.
const POLL: Duration = Duration::from_millis(25);
/// The poll period once TERM has been sent, until the child exits. A writer can add well over 1 MB per millisecond,
/// so the overrun before the KILL stays small, and the recorded end is close to the real one.
const POLL_STOPPING: Duration = Duration::from_millis(1);
/// Cancel requests, the grace period and sampling run at this period.
const TICK: Duration = Duration::from_millis(500);
/// The process tree is sampled on every fourth tick: first at ~1.5 s, then every 2 s.
const SAMPLE_EVERY: u64 = 4;
/// Time between the first TERM to the process group and the KILL.
const KILL_AFTER: Duration = Duration::from_secs(5);
/// Once TERM has been sent for the log cap, a log that grows this far beyond the cap gets KILL at once.
const LOG_OVERRUN: u64 = 16 << 20;
/// Width of the command text in "work: cannot run ..." lines.
const BRIEF_LIMIT: usize = 200;
/// ENOENT, the errno reported for an empty argv.
const ENOENT: i32 = 2;

/// `detach` is production: setsid and the signal flags. Tests run the same core without them, because setsid on
/// the test runner would detach it and signal flags would be process-wide.
#[derive(Debug, Clone, Copy)]
struct Opts {
    detach: bool,
}

/// Parses ["--job-dir", D]; returns the process exit code (0 ok, 1 failure to supervise, 2 usage).
pub fn main_from_args(args: &[String]) -> i32 {
    let dir = match args {
        [flag, dir] if flag.as_str() == "--job-dir" => PathBuf::from(dir),
        _ => {
            eprintln!("agora: _supervise is started by the agora daemon");
            return 2;
        }
    };
    match supervise(&JobDir { dir }) {
        Ok(_) => 0,
        Err(error) => {
            eprintln!("agora: supervise failed: {error}");
            1
        }
    }
}

/// Runs the job in `dir`, records status.json and returns it. A failure to supervise is recorded too (see `run_job`).
pub fn supervise(dir: &JobDir) -> Result<JobStatus, RunError> {
    run_job(dir, Opts { detach: true })
}

/// Records every failure, so the daemon never waits on a job that has no end. If no status.json exists after an
/// error, it is written with the note "could not supervise: <error>"; the error is still returned to the caller.
fn run_job(dir: &JobDir, opts: Opts) -> Result<JobStatus, RunError> {
    let cancel_signal = Arc::new(AtomicBool::new(false));
    match run_job_inner(dir, opts, &cancel_signal) {
        Ok(status) => Ok(status),
        Err(error) => {
            if !matches!(dir.read_status(), Ok(Some(_))) {
                let note = format!("could not supervise: {error}");
                // A failed write here leaves no status, which is no worse than before; the original error still wins.
                let _ = dir.write_status(&unstarted(None, note));
            }
            Err(error)
        }
    }
}

/// setsid; read spec; if the cwd is missing, cannot be entered or no longer resolves to itself -> status None,
/// unstarted, with the note; write the
/// context_start line (exclusive) to the log; locate -> (126/127 + note "could not start: <error>", log line
/// "work: cannot run <brief>: <error>"); qos::wrap; spawn with process_group(0), stdin null, stdout/stderr = log;
/// write supervisor.json; watch the child (see `supervise_child`); then the CPU time and peak RSS of the child from
/// getrusage, diffed against a baseline taken before the launch; note "stopped: its log passed the limit of N bytes
/// (agora work settings --log-limit)" | ""; exclusive: append "work: context at end: ..." line; write status.json.
fn run_job_inner(
    dir: &JobDir,
    opts: Opts,
    cancel_signal: &Arc<AtomicBool>,
) -> Result<JobStatus, RunError> {
    if opts.detach {
        detach(cancel_signal)?;
    }
    let spec = dir.read_spec()?;
    let mut log = open_log(&spec.log)?;
    if let Some(problem) = cwd_problem(&spec.cwd) {
        line(
            &mut log,
            &format!(
                "work: cannot run {}: {problem}",
                brief(&spec.argv, BRIEF_LIMIT)
            ),
        );
        return finish(dir, unstarted(None, problem));
    }
    if let Some(text) = &spec.context_start {
        line(&mut log, text);
    }
    // An exclusive job's end line reports the idle CPU over the run, so the probe is primed right before launch.
    let machine = spec.exclusive.then(|| RealMachine::new(dir.dir.clone()));
    if let Some(machine) = &machine {
        let _ = machine.snapshot();
    }
    let baseline = children().ok();
    let (mut child, qos) = match launch(&spec, &log)? {
        Outcome::Running { child, qos } => (child, qos),
        Outcome::Refused { status, error } => {
            line(
                &mut log,
                &format!(
                    "work: cannot run {}: {error}",
                    brief(&spec.argv, BRIEF_LIMIT)
                ),
            );
            return finish(
                dir,
                unstarted(Some(status), format!("could not start: {error}")),
            );
        }
    };
    let started_ms = now_ms();
    publish(dir, &child, started_ms, &qos, &mut log);
    let watched = supervise_child(dir, &spec, &mut child, &log, cancel_signal)?;
    let after = children().ok();
    let cpu_seconds = match (baseline, after) {
        (Some(before), Some(after)) => {
            let micros = after.cpu_micros.saturating_sub(before.cpu_micros);
            Some(micros as f64 / 1_000_000.0)
        }
        _ => None,
    };
    let rss_peak_kib = watched
        .sampled_rss_peak_kib
        .into_iter()
        .chain(after.map(|usage| usage.max_rss_kib))
        .max();
    if let Some(machine) = &machine {
        let snapshot = machine.snapshot();
        let signals = snapshot.signals.as_ref();
        let load = signals.and_then(|s| s.load1);
        let idle = signals.and_then(|s| s.idle_pct);
        line(&mut log, &end_context(load, idle));
    }
    let note = if watched.over_limit {
        over_limit_note(spec.log_limit)
    } else {
        String::new()
    };
    finish(
        dir,
        JobStatus {
            status: exit_status(watched.exit.code(), watched.exit.signal()),
            note,
            cpu_seconds,
            rss_peak_kib,
            started_ms: Some(started_ms),
            ended_ms: watched.ended_ms,
            started: true,
            cancelled: watched.cancelled,
            over_limit: watched.over_limit,
        },
    )
}

/// What the watch loop saw: how the child ended and when it was reaped, and the limits and requests that stopped it.
struct Watched {
    exit: ExitStatus,
    ended_ms: i64,
    over_limit: bool,
    cancelled: bool,
    sampled_rss_peak_kib: Option<u64>,
}

/// Polls the child until it is reaped, timestamping the reap before anything else runs, runs the log-cap check on
/// every poll and the housekeeping on the tick. A child that cannot be waited for is killed, so it does not outlive the supervisor.
fn supervise_child(
    dir: &JobDir,
    spec: &JobSpec,
    child: &mut Child,
    log: &File,
    cancel_signal: &AtomicBool,
) -> Result<Watched, RunError> {
    let pid = child.id();
    let mut sampler = Sampler::new();
    // The first CPU percent is a delta from the previous call, so the sampler is primed right after the spawn.
    let _ = sampler.sample_tree(pid);
    let mut watch = Watch {
        dir,
        spec,
        log,
        cancel_signal,
        pid,
        sampler,
        ticks: 0,
        over_limit: false,
        cancelled: false,
        term_at: None,
        killed: false,
        rss_peak_kib: None,
    };
    let mut next_tick = Instant::now();
    let (exit, ended_ms) = loop {
        match child.try_wait() {
            Ok(Some(exit)) => break (exit, now_ms()),
            Ok(None) => {}
            Err(error) => {
                let _ = signal_group(pid, Sig::Kill);
                return Err(error.into());
            }
        }
        let now = Instant::now();
        watch.poll(now);
        if now >= next_tick {
            next_tick = now + TICK;
            watch.tick(now);
        }
        thread::sleep(poll_period(watch.term_at.is_some()));
    };
    // The leader is gone, but a member of its group may still run (one that ignored TERM, for example).
    if watch.term_at.is_some() && !watch.killed {
        let _ = signal_group(pid, Sig::Kill);
    }
    Ok(Watched {
        exit,
        ended_ms,
        over_limit: watch.over_limit,
        cancelled: watch.cancelled,
        sampled_rss_peak_kib: watch.rss_peak_kib,
    })
}

/// The time between two polls: the normal period, or the short one once TERM has been sent (`term_sent`).
fn poll_period(term_sent: bool) -> Duration {
    if term_sent { POLL_STOPPING } else { POLL }
}

/// The state of one watched job, and the checks that run on each poll and the housekeeping that runs on each tick.
struct Watch<'a> {
    dir: &'a JobDir,
    spec: &'a JobSpec,
    log: &'a File,
    cancel_signal: &'a AtomicBool,
    pid: u32,
    sampler: Sampler,
    ticks: u64,
    over_limit: bool,
    cancelled: bool,
    term_at: Option<Instant>,
    killed: bool,
    rss_peak_kib: Option<u64>,
}

impl Watch<'_> {
    /// The log's length, or None if it cannot be read (the checks then leave the job alone for this poll).
    fn log_len(&self) -> Option<u64> {
        self.log.metadata().ok().map(|meta| meta.len())
    }

    /// Runs on every poll: the log cap (TERM once it is passed), and, once TERM is out, KILL at once if the log has
    /// grown LOG_OVERRUN beyond the cap, so a writer that ignores TERM cannot keep filling the disk for the grace period.
    fn poll(&mut self, now: Instant) {
        let size = self.log_len();
        if !self.over_limit && size.is_some_and(|size| size > self.spec.log_limit) {
            self.over_limit = true;
        }
        self.terminate_if_requested(now);
        let overrun = self.spec.log_limit.saturating_add(LOG_OVERRUN);
        if self.term_at.is_some() && !self.killed && size.is_some_and(|size| size > overrun) {
            self.kill();
        }
    }

    /// Runs on the tick: the cancel requests, KILL after the grace period, and on every fourth tick a sample of the
    /// process tree.
    fn tick(&mut self, now: Instant) {
        self.ticks += 1;
        if !self.cancelled
            && (self.dir.cancel_requested() || self.cancel_signal.load(Ordering::SeqCst))
        {
            self.cancelled = true;
        }
        self.terminate_if_requested(now);
        if let Some(at) = self.term_at
            && !self.killed
            && now.duration_since(at) >= KILL_AFTER
        {
            self.kill();
        }
        if self.ticks.is_multiple_of(SAMPLE_EVERY)
            && let Some(tree) = self.sampler.sample_tree(self.pid)
        {
            let peak = self.rss_peak_kib.unwrap_or(0).max(tree.rss_kib);
            self.rss_peak_kib = Some(peak);
            // A failed sample write must not stop the job being supervised; the next sample replaces it.
            let _ = self.dir.write_sample(&Sample {
                cpu_percent: tree.cpu_percent,
                rss_kib: tree.rss_kib,
                rss_peak_kib: peak,
                sampled_ms: now_ms(),
            });
        }
    }

    /// Sends TERM to the group once, when the log cap is passed or a cancel is requested.
    fn terminate_if_requested(&mut self, now: Instant) {
        if (self.over_limit || self.cancelled) && self.term_at.is_none() {
            self.term_at = Some(now);
            let _ = signal_group(self.pid, Sig::Term);
        }
    }

    fn kill(&mut self) {
        self.killed = true;
        let _ = signal_group(self.pid, Sig::Kill);
    }
}

enum Outcome {
    Running { child: Child, qos: String },
    Refused { status: i32, error: String },
}

/// Locates the program first, so that a missing or non-runnable one is 127 or 126 rather than the wrapper's status,
/// then wraps it for QoS and spawns it in its own process group with stdin closed and the log as stdout and stderr.
fn launch(spec: &JobSpec, log: &File) -> Result<Outcome, RunError> {
    let Some((program, args)) = spec.argv.split_first() else {
        return Ok(refused(ENOENT));
    };
    let path_env = spec.env.get("PATH").map(String::as_str);
    let resolved = match locate(program, Path::new(&spec.cwd), path_env) {
        Ok(path) => path,
        Err(errno) => return Ok(refused(errno)),
    };
    let (argv, qos) = qos::wrap(spec.qos, spec.exclusive, &resolved, args);
    let Some((exec, rest)) = argv.split_first() else {
        return Ok(refused(ENOENT));
    };
    let mut command = Command::new(exec);
    command
        .args(rest)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(&spec.env)
        .env("PWD", &spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?))
        .process_group(0);
    match command.spawn() {
        Ok(child) => Ok(Outcome::Running { child, qos }),
        Err(error) => Ok(Outcome::Refused {
            status: spawn_error_status(error.raw_os_error().unwrap_or(0)),
            error: error.to_string(),
        }),
    }
}

fn refused(errno: i32) -> Outcome {
    Outcome::Refused {
        status: spawn_error_status(errno),
        error: io::Error::from_raw_os_error(errno).to_string(),
    }
}

/// Why the job cannot start in `cwd`, as the note and the log line word it. A missing directory is gone; one that
/// exists but cannot be entered (no search permission, or not a directory) is refused. A `cwd` that is not its own
/// canonical path (a symlink, say, that was re-pointed after the job was accepted) is refused too, so the job never
/// runs in a directory other than the one it was recorded with. None when it can be entered.
fn cwd_problem(cwd: &str) -> Option<String> {
    match std::fs::metadata(cwd) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Some(format!("cannot enter its directory {cwd}: not a directory")),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Some(format!("its directory {cwd} is gone"));
        }
        Err(error) => return Some(format!("cannot enter its directory {cwd}: {error}")),
    }
    if let Some(errno) = access(cwd, Access::EXEC_OK).err() {
        return Some(format!(
            "cannot enter its directory {cwd}: {}",
            io::Error::from(errno)
        ));
    }
    match std::fs::canonicalize(cwd) {
        Ok(real) if real.as_os_str() == cwd => None,
        Ok(real) => Some(format!(
            "its directory {cwd} now resolves to {}",
            real.display()
        )),
        Err(error) => Some(format!("cannot enter its directory {cwd}: {error}")),
    }
}

/// Production start-up: a new session (EPERM, already a session leader, is fine), and flags for TERM, INT and HUP.
fn detach(cancel_signal: &Arc<AtomicBool>) -> Result<(), RunError> {
    if let Err(errno) = setsid()
        && errno != Errno::PERM
    {
        return Err(io::Error::from(errno).into());
    }
    for signal in [SIGTERM, SIGINT, SIGHUP] {
        signal_hook::flag::register(signal, Arc::clone(cancel_signal))?;
    }
    Ok(())
}

/// Opens the log the daemon created, for append. A symlink is refused (NOFOLLOW), and so is anything that is not a
/// regular file: O_NONBLOCK keeps a FIFO from blocking the open, and fstat then refuses it.
fn open_log(path: &str) -> Result<File, RunError> {
    let flags =
        OFlags::WRONLY | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = open(path, flags, Mode::empty()).map_err(io::Error::from)?;
    let stat = fstat(&fd).map_err(io::Error::from)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(RunError::Invalid(format!(
            "{path} is not a regular file; refusing to write the job log there"
        )));
    }
    Ok(File::from(fd))
}

/// Log writes are diagnostics: a failed write must not stop the job from being recorded.
fn line(log: &mut File, text: &str) {
    let _ = writeln!(log, "{text}");
}

fn publish(dir: &JobDir, child: &Child, started_ms: i64, qos: &str, log: &mut File) {
    let own = std::process::id();
    let info = SupervisorInfo {
        supervisor_pid: own,
        supervisor_start_s: start_time_s(own),
        pid: Some(child.id()),
        pid_start_s: start_time_s(child.id()),
        started_ms,
        qos: qos.to_owned(),
    };
    if let Err(error) = dir.write_info(&info) {
        line(
            log,
            &format!("work: could not record the supervisor: {error}"),
        );
    }
}

fn unstarted(status: Option<i32>, note: String) -> JobStatus {
    JobStatus {
        status,
        note,
        cpu_seconds: None,
        rss_peak_kib: None,
        started_ms: None,
        ended_ms: now_ms(),
        started: false,
        cancelled: false,
        over_limit: false,
    }
}

fn finish(dir: &JobDir, status: JobStatus) -> Result<JobStatus, RunError> {
    dir.write_status(&status)?;
    Ok(status)
}

fn over_limit_note(limit: u64) -> String {
    format!("stopped: its log passed the limit of {limit} bytes (agora work settings --log-limit)")
}

/// The exclusive job's end line. The running jobs are not listed: the supervisor has no database to read them from.
fn end_context(load: Option<f64>, idle: Option<f64>) -> String {
    let load = load.map_or_else(|| "unknown".to_owned(), |load| format!("{load:.2}"));
    let idle = idle.map_or_else(
        || "idle unknown".to_owned(),
        |idle| format!("{idle:.0}% idle"),
    );
    format!("work: context at end: load {load}, CPU {idle} over the run")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
