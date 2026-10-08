//! Job directory layout under `$AGORA_HOME/work/`: `jobs/<id>/` (0700: spec.json, supervisor.json, sample.json,
//! status.json, cancel) and `logs/<id>.log` (0600). All JSON files are written atomically (tmp file in the same dir,
//! 0600, fsync, rename) and read with O_NOFOLLOW, a regular-file check and a 1 MiB cap.

use std::collections::BTreeMap;
use std::fs::{DirBuilder, File, Metadata, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::OFlags;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::RunError;

/// Directories are owner-only.
const DIR_MODE: u32 = 0o700;
/// Files are owner-only.
const FILE_MODE: u32 = 0o600;
/// Largest JSON file read back.
const MAX_JSON_BYTES: u64 = 1024 * 1024;
/// How much of the end of a log `tail` reads.
const TAIL_BYTES: u64 = 64 * 1024;
const O_NOFOLLOW: i32 = OFlags::NOFOLLOW.bits() as i32;
const O_TRUNC: i32 = OFlags::TRUNC.bits() as i32;
/// Keeps a FIFO planted at a read path from blocking the open; the regular-file check then rejects it.
const O_NONBLOCK: i32 = OFlags::NONBLOCK.bits() as i32;
/// Distinguishes the temp files of concurrent writes within one process.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    pub task: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub log: String,
    pub log_limit: u64,
    pub qos: agora_sched::settings::Qos,
    pub exclusive: bool,
    /// exclusive jobs: the "work: context at start: ..." line, written to the log before launch.
    pub context_start: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupervisorInfo {
    pub supervisor_pid: u32,
    pub supervisor_start_s: Option<u64>,
    pub pid: Option<u32>,
    pub pid_start_s: Option<u64>,
    pub started_ms: i64,
    pub qos: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub cpu_percent: f64,
    pub rss_kib: u64,
    pub rss_peak_kib: u64,
    pub sampled_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobStatus {
    pub status: Option<i32>,
    pub note: String,
    pub cpu_seconds: Option<f64>,
    pub rss_peak_kib: Option<u64>,
    pub started_ms: Option<i64>,
    pub ended_ms: i64,
    pub started: bool,
    pub cancelled: bool,
    pub over_limit: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobDir {
    pub dir: PathBuf,
}

impl JobDir {
    /// <work_root>/jobs/<task>; task must pass agora_sched::claims::validate_name.
    pub fn new(work_root: &Path, task: &str) -> Result<JobDir, RunError> {
        agora_sched::claims::validate_name("task id", task).map_err(RunError::Invalid)?;
        Ok(JobDir {
            dir: work_root.join("jobs").join(task),
        })
    }

    /// mkdir -p 0700; refuses a symlink.
    pub fn create(&self) -> Result<(), RunError> {
        let jobs = self.dir.parent().ok_or_else(|| no_parent(&self.dir))?;
        let root = jobs.parent().ok_or_else(|| no_parent(jobs))?;
        ensure_private_dir(root)?;
        ensure_private_dir(jobs)?;
        ensure_private_dir(&self.dir)
    }

    pub fn write_spec(&self, s: &JobSpec) -> Result<(), RunError> {
        self.write_json("spec.json", s)
    }

    pub fn read_spec(&self) -> Result<JobSpec, RunError> {
        self.read_json("spec.json")?
            .ok_or_else(|| missing("spec.json"))
    }

    pub fn write_info(&self, i: &SupervisorInfo) -> Result<(), RunError> {
        self.write_json("supervisor.json", i)
    }

    pub fn read_info(&self) -> Result<Option<SupervisorInfo>, RunError> {
        self.read_json("supervisor.json")
    }

    pub fn write_sample(&self, s: &Sample) -> Result<(), RunError> {
        self.write_json("sample.json", s)
    }

    pub fn read_sample(&self) -> Result<Option<Sample>, RunError> {
        self.read_json("sample.json")
    }

    pub fn write_status(&self, s: &JobStatus) -> Result<(), RunError> {
        self.write_json("status.json", s)
    }

    pub fn read_status(&self) -> Result<Option<JobStatus>, RunError> {
        self.read_json("status.json")
    }

    pub fn request_cancel(&self) -> Result<(), RunError> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(FILE_MODE)
            .custom_flags(O_NOFOLLOW)
            .open(self.dir.join("cancel"))?;
        Ok(())
    }

    pub fn cancel_requested(&self) -> bool {
        std::fs::symlink_metadata(self.dir.join("cancel")).is_ok()
    }

    /// removes the dir (lstat: real dir, not a symlink) and its files. A missing dir is already removed.
    pub fn remove(&self) -> Result<(), RunError> {
        match std::fs::symlink_metadata(&self.dir) {
            Ok(meta) => check_real_dir(&self.dir, &meta)?,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        std::fs::remove_dir_all(&self.dir)?;
        Ok(())
    }

    /// Temp file in the same dir, fsync, rename over the target. A failed write leaves no temp file behind.
    fn write_json<T: Serialize>(&self, name: &str, value: &T) -> Result<(), RunError> {
        let bytes = serde_json::to_vec(value)?;
        let target = self.dir.join(name);
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .dir
            .join(format!("{name}.tmp-{}-{seq}", std::process::id()));
        let mut file = open_new(&tmp)?;
        let written = file
            .write_all(&bytes)
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::rename(&tmp, &target));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(written?)
    }

    /// `Ok(None)` when the file is missing. A symlink fails the open; anything but a regular file is refused.
    fn read_json<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, RunError> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(self.dir.join(name))
        {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let bytes = read_regular(&file, name)?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }
}

/// <home>/work
pub fn work_root(agora_home: &Path) -> PathBuf {
    agora_home.join("work")
}

/// <work_root>/logs/<task>.log
pub fn log_path(work_root: &Path, task: &str) -> PathBuf {
    work_root.join("logs").join(format!("{task}.log"))
}

/// O_WRONLY|O_CREAT|O_TRUNC|O_APPEND|O_NOFOLLOW, 0600 (creates logs/ 0700 if needed).
pub fn create_log(path: &Path) -> Result<File, RunError> {
    let logs = path.parent().ok_or_else(|| no_parent(path))?;
    ensure_private_dir(logs)?;
    // std refuses `truncate` with `append`, so O_TRUNC goes through custom_flags like O_NOFOLLOW.
    Ok(OpenOptions::new()
        .create(true)
        .append(true)
        .mode(FILE_MODE)
        .custom_flags(O_NOFOLLOW | O_TRUNC)
        .open(path)?)
}

/// last `lines` lines of the final 64 KiB, utf-8 lossy, split like Python str.splitlines (\r, \n, \r\n), each through
/// agora_sched::pyfmt::printable. A missing or unreadable file gives no lines, as in work.py.
pub fn tail(path: &Path, lines: usize) -> Vec<String> {
    if lines == 0 {
        return Vec::new();
    }
    let Some(text) = read_tail_text(path) else {
        return Vec::new();
    };
    let all = split_py_lines(&text);
    let skip = all.len().saturating_sub(lines);
    all.into_iter()
        .skip(skip)
        .map(agora_sched::pyfmt::printable)
        .collect()
}

fn no_parent(path: &Path) -> RunError {
    RunError::Invalid(format!("{} has no parent directory", path.display()))
}

fn missing(name: &str) -> RunError {
    RunError::Io(std::io::Error::new(
        ErrorKind::NotFound,
        format!("{name} is missing"),
    ))
}

fn check_real_dir(path: &Path, meta: &Metadata) -> Result<(), RunError> {
    if meta.file_type().is_symlink() {
        Err(RunError::Invalid(format!(
            "{} is a symbolic link; refusing to use it",
            path.display()
        )))
    } else if !meta.is_dir() {
        Err(RunError::Invalid(format!(
            "{} is not a directory",
            path.display()
        )))
    } else {
        Ok(())
    }
}

/// Creates `path` with mode 0700 when absent; an existing symlink or non-directory is refused.
fn ensure_private_dir(path: &Path) -> Result<(), RunError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => check_real_dir(path, &meta),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            match DirBuilder::new().mode(DIR_MODE).create(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    check_real_dir(path, &std::fs::symlink_metadata(path)?)
                }
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Exclusive create: fails on an existing name, and never follows a symlink.
fn open_new(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .custom_flags(O_NOFOLLOW)
        .open(path)
}

/// Reads a regular file of at most 1 MiB.
fn read_regular(file: &File, name: &str) -> Result<Vec<u8>, RunError> {
    if !file.metadata()?.is_file() {
        return Err(RunError::Invalid(format!("{name} is not a regular file")));
    }
    let mut bytes = Vec::new();
    let mut limited = file.take(MAX_JSON_BYTES + 1);
    limited.read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_JSON_BYTES {
        return Err(RunError::Invalid(format!("{name} is larger than 1 MiB")));
    }
    Ok(bytes)
}

/// The final 64 KiB of a regular file, decoded lossily; `None` when it cannot be read.
fn read_tail_text(path: &Path) -> Option<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    file.seek(SeekFrom::Start(meta.len().saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut bytes = Vec::new();
    let mut limited = file.take(TAIL_BYTES);
    limited.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The characters at which Python's `str.splitlines` ends a line.
fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\x0b'
            | '\x0c'
            | '\x1c'
            | '\x1d'
            | '\x1e'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// Python `str.splitlines()`: `\r\n` is one break, and a final break adds no empty line.
fn split_py_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if !is_line_break(c) {
            continue;
        }
        let mut end = at + c.len_utf8();
        if c == '\r' && chars.peek().is_some_and(|&(_, next)| next == '\n') {
            chars.next();
            end += 1;
        }
        lines.push(text.get(start..at).unwrap_or_default());
        start = end;
    }
    if let Some(rest) = text.get(start..).filter(|rest| !rest.is_empty()) {
        lines.push(rest);
    }
    lines
}

#[cfg(test)]
mod tests;
