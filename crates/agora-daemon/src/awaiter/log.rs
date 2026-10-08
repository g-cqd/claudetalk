//! The log condition: a pattern matched against the lines a log gains, scanned on the blocking pool.

use super::cursor::Cursor;
use super::rules::compile_pattern;
use super::turn::{Ctx, Found, Wake};
use crate::{DaemonError, watch};
use agora_run::jobdir;
use agora_sched::model::WaitResult;
use std::path::PathBuf;

/// Log lines returned on a match or a timeout.
const LOG_TAIL_LINES: usize = 5;

/// Log: done when a line (or the trailing partial line) matches `pattern`. Scans resume from the cursor's offset
/// and carry; a timeout returns the new ones. The tail is returned on a match and on a timeout, and a timeout lists
/// the log as pending (D10).
pub(super) async fn log_wait(
    ctx: &Ctx<'_>,
    path: String,
    pattern: String,
    resumed: Option<Cursor>,
) -> Result<WaitResult, DaemonError> {
    let re = compile_pattern(&pattern)?;
    let file = PathBuf::from(&path);
    let (mut offset, mut carry) = resumed.map_or((0, String::new()), |c| (c.offset, c.carry));
    let mut wake = Wake::Fs(watch::Watcher::new(std::slice::from_ref(&file)));
    loop {
        let (next, next_carry, matched) =
            scan(file.clone(), offset, carry.clone(), re.clone()).await?;
        offset = next;
        carry = next_carry;
        if matched {
            return Ok(ctx.end(
                false,
                Found {
                    targets: vec![path],
                    tail: jobdir::tail(&file, LOG_TAIL_LINES),
                    ..Found::default()
                },
            ));
        }
        if !ctx.wait_turn(&mut wake, None).await? {
            let cursor = Cursor {
                offset,
                carry: carry.clone(),
                ..Cursor::default()
            };
            return Ok(ctx.end(
                true,
                Found {
                    targets: vec![path.clone()],
                    pending: vec![path],
                    tail: jobdir::tail(&file, LOG_TAIL_LINES),
                    cursor,
                    ..Found::default()
                },
            ));
        }
    }
}

/// One log scan on the blocking pool: it reads up to 16 MiB.
async fn scan(
    file: PathBuf,
    offset: u64,
    carry: String,
    re: regex::Regex,
) -> Result<(u64, String, bool), DaemonError> {
    tokio::task::spawn_blocking(move || watch::log_scan(&file, offset, &carry, &re))
        .await
        .map_err(|e| DaemonError::Io(std::io::Error::other(e)))
}
