//! Helpers for the awaiter and for job adoption: filesystem change notification, log scanning, pid probes.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{RecursiveMode, Watcher as _};
use rustix::process::{Pid, test_kill_process};
use tracing::debug;

/// The longest poll used when no watcher can be created (the documented fallback of D10).
const FALLBACK_POLL: Duration = Duration::from_secs(1);
/// The most bytes one `log_scan` call reads (D10).
const MAX_READ: u64 = 16 * 1024 * 1024;
/// The most bytes of an unfinished line kept between `log_scan` calls. The awaiter's cursor carries these bytes as
/// base64 inside its JSON, so the cap keeps every cursor well inside one frame.
pub const MAX_CARRY: usize = 64 * 1024;

/// Watches paths for changes: one notify watcher per distinct parent directory, all feeding one channel.
///
/// Create it before the first check of a condition and keep it for the whole wait: an event that arrives between the
/// check and the next `changed` call is queued, not lost. If any parent directory cannot be watched (or there are no
/// paths), the watcher is in polling mode and `changed` sleeps for at most 1 s.
pub struct Watcher {
    /// Keeps the notify watchers alive; they stop when this is dropped.
    _watchers: Vec<notify::RecommendedWatcher>,
    /// Events from every watched directory; `None` in polling mode.
    events: Option<tokio::sync::mpsc::UnboundedReceiver<notify::Result<notify::Event>>>,
    /// The paths an event must name to count: each path as given, and as its canonical directory joins it.
    targets: Vec<PathBuf>,
}

impl Watcher {
    /// A watcher for `paths`. Only events naming one of them count, or events with no path at all (a rescan); other
    /// files in the same directories are ignored. Watcher errors are ignored: the caller re-checks its condition after
    /// every return, so a missed event costs at most one `changed` cadence.
    pub fn new(paths: &[PathBuf]) -> Watcher {
        Watcher::open(paths).unwrap_or_else(|| Watcher {
            _watchers: Vec::new(),
            events: None,
            targets: Vec::new(),
        })
    }

    /// The watchers for `paths`; `None` when there are none, or when any of them cannot be created.
    fn open(paths: &[PathBuf]) -> Option<Watcher> {
        if paths.is_empty() {
            return None;
        }
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut targets = Vec::new();
        for path in paths {
            let Some((dir, target)) = watch_target(path) else {
                debug!(path = %path.display(), "no parent directory to watch; polling");
                return None;
            };
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
            targets.push(target);
            targets.push(path.clone());
        }
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        let mut watchers = Vec::with_capacity(dirs.len());
        for dir in &dirs {
            let tx = tx.clone();
            // The callback runs on notify's own thread. A dropped receiver only means nobody waits any more.
            let created =
                notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    let _ = tx.send(event);
                });
            let mut watcher = match created {
                Ok(watcher) => watcher,
                Err(err) => {
                    debug!(%err, "cannot create a file watcher; polling");
                    return None;
                }
            };
            if let Err(err) = watcher.watch(dir, RecursiveMode::NonRecursive) {
                debug!(%err, dir = %dir.display(), "cannot watch the directory; polling");
                return None;
            }
            watchers.push(watcher);
        }
        Some(Watcher {
            _watchers: watchers,
            events: Some(events),
            targets,
        })
    }

    /// Resolves when an event names one of the paths, or after `max`, whichever comes first. In polling mode it sleeps
    /// for `max`, or for 1 s when that is shorter. Queued events are consumed, so a later call starts from new ones.
    pub async fn changed(&mut self, max: Duration) {
        let targets = &self.targets;
        let Some(events) = self.events.as_mut() else {
            return tokio::time::sleep(max.min(FALLBACK_POLL)).await;
        };
        let _ = tokio::time::timeout(max, async {
            loop {
                match events.recv().await {
                    Some(Ok(event)) if names_target(&event, targets) => return,
                    Some(_) => {}
                    // The sender lives in the watchers, which outlive this loop, so this arm is not reached. If it
                    // ever is, the deadline still bounds the wait.
                    None => std::future::pending::<()>().await,
                }
            }
        })
        .await;
    }
}

/// Resolves when `path` changes state (created, written, removed) or after `max`, whichever comes first: a one-shot
/// [`Watcher`] on `path`. Used by job adoption, which waits on a single directory.
pub async fn changed(path: &Path, max: Duration) {
    Watcher::new(&[path.to_path_buf()]).changed(max).await;
}

/// Whether an event names one of the targets, or names no path at all (a rescan).
fn names_target(event: &notify::Event, targets: &[PathBuf]) -> bool {
    event.paths.is_empty() || event.paths.iter().any(|p| targets.contains(p))
}

/// The directory to watch, and the path as the watcher reports it. The directory is canonicalized because FSEvents
/// reports `/private/var/...` for `/var/...`. `None` when the parent cannot be resolved, which makes the caller poll.
fn watch_target(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let name = path.file_name()?;
    let parent = match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
        Some(parent) => parent,
        None => return None,
    };
    let dir = parent.canonicalize().ok()?;
    let target = dir.join(name);
    Some((dir, target))
}

/// The file exists and its size is greater than zero. A directory, or anything that is not a regular file, is not
/// ready.
pub fn file_ready(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() > 0)
}

/// Read new bytes from `offset` (at most 16 MiB per call, carry at most 64 KiB); returns
/// `(new_offset, new_carry, matched)`.
///
/// Every complete line is tested with `re`, and so is the trailing partial line (work.py matches partial last lines
/// too); the partial line becomes the new carry, keeping its last 64 KiB. A trailing incomplete UTF-8 sequence is not
/// read yet: its bytes stay before the new offset, so the next call decodes the character whole. A missing file, a
/// file that is not regular, a symlink (`O_NOFOLLOW`), or a file that cannot be read leaves `(offset, carry, false)`
/// unchanged. A file shorter than `offset` was truncated: the scan restarts at 0 with an empty carry. The read is
/// blocking file I/O.
pub fn log_scan(path: &Path, offset: u64, carry: &str, re: &regex::Regex) -> (u64, String, bool) {
    let Ok(mut file) = open_log(path) else {
        return (offset, carry.to_owned(), false);
    };
    let Ok(meta) = file.metadata() else {
        return (offset, carry.to_owned(), false);
    };
    if !meta.is_file() {
        return (offset, carry.to_owned(), false);
    }
    let len = meta.len();
    let (offset, mut text) = if len < offset {
        (0, String::new())
    } else {
        (offset, carry.to_owned())
    };
    let Ok(bytes) = read_chunk(&mut file, offset, len) else {
        return (offset, text, false);
    };
    let new_offset = offset.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    text.push_str(&String::from_utf8_lossy(&bytes));
    let (matched, partial) = scan_lines(&text, re);
    (new_offset, keep_tail(partial, MAX_CARRY), matched)
}

/// Opens a log for reading with `O_NOFOLLOW` (a symlink fails with ELOOP) and `O_NONBLOCK` (opening a FIFO with no
/// writer would otherwise block the caller). The libc constants are used, as the other `O_NOFOLLOW` opens in this
/// crate do; the caller rejects anything that is not a regular file.
fn open_log(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

/// Reads at most `MAX_READ` bytes of `file` from `offset`, where `len` is the file's current length. Trailing bytes of
/// an incomplete UTF-8 sequence are left for the next call, always: a character split by the cap, or by the end of
/// the file while it is being written, is decoded whole once its last byte arrives.
fn read_chunk(file: &mut File, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    let wanted = len.saturating_sub(offset).min(MAX_READ);
    let mut buf = Vec::new();
    file.seek(SeekFrom::Start(offset))?;
    Read::take(&*file, wanted).read_to_end(&mut buf)?;
    buf.truncate(complete_utf8_len(&buf));
    Ok(buf)
}

/// The length of the longest prefix of `bytes` that does not end in an incomplete UTF-8 sequence (at most three bytes
/// are cut). Invalid bytes elsewhere are kept: they decode lossily.
fn complete_utf8_len(bytes: &[u8]) -> usize {
    let mut rest = bytes;
    let mut kept = 0;
    loop {
        match std::str::from_utf8(rest) {
            Ok(_) => return bytes.len(),
            Err(err) => {
                let valid = err.valid_up_to();
                match err.error_len() {
                    None => return kept + valid,
                    Some(bad) => {
                        kept += valid + bad;
                        rest = rest.get(valid + bad..).unwrap_or(&[]);
                    }
                }
            }
        }
    }
}

/// Tests every complete line and the trailing partial line; returns whether any tested line matched, and the partial
/// line (the text after the last newline, possibly empty). An empty partial line is not a line, so it is not tested:
/// `^$` must not match the end of a file.
fn scan_lines<'a>(text: &'a str, re: &regex::Regex) -> (bool, &'a str) {
    let mut matched = false;
    let mut partial = "";
    let mut pieces = text.split('\n').peekable();
    while let Some(piece) = pieces.next() {
        if pieces.peek().is_some() {
            matched |= re.is_match(piece);
        } else {
            partial = piece;
            matched |= !piece.is_empty() && re.is_match(piece);
        }
    }
    (matched, partial)
}

/// The last `max` bytes of `line`, moved forward to a character boundary.
fn keep_tail(line: &str, max: usize) -> String {
    if line.len() <= max {
        return line.to_owned();
    }
    let mut start = line.len() - max;
    while !line.is_char_boundary(start) {
        start += 1;
    }
    line.get(start..).unwrap_or("").to_owned()
}

/// The process exists (a rustix test); another user's process counts as exited.
///
/// Zero and negative values are refused before the kernel sees them: `kill(0, _)` and `kill(-1, _)` address whole
/// process groups or every process. A zombie answers `kill(pid, 0)` but has exited, so when the test succeeds the
/// live check (`agora_run::proc::alive`, which excludes zombies) decides. `EPERM` (another user's process) and
/// `ESRCH` (no such process) both mean exited, and so does any other error, since the process cannot be shown alive.
pub fn pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    let Some(os_pid) = Pid::from_raw(raw) else {
        return false;
    };
    match test_kill_process(os_pid) {
        Ok(()) => u32::try_from(pid).is_ok_and(|pid| agora_run::proc::alive(pid, None)),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests;
