use std::io::{self, ErrorKind};

use rustix::fs::{Access, access};

/// Why the job cannot start in `cwd`, as the note and the log line word it. A missing directory is gone; one that
/// exists but cannot be entered (no search permission, or not a directory) is refused. A `cwd` that is not its own
/// canonical path (a symlink, say, that was re-pointed after the job was accepted) is refused too, so the job never
/// runs in a directory other than the one it was recorded with. None when it can be entered.
pub(super) fn cwd_problem(cwd: &str) -> Option<String> {
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
