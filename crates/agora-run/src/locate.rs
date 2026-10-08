//! execvp emulation (ref-queue-cli section 4 locate()): PATH search, magic-byte check, ENOEXEC for shebang-less scripts.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rustix::io::Errno;

/// The PATH searched when the caller has none (the spec fixes this instead of os.defpath).
const DEFAULT_PATH: &str = "/usr/bin:/bin";

/// The first bytes of a file that exec runs: a script's #!, and Mach-O, universal and ELF binaries.
const EXEC_MAGIC: &[&[u8]] = &[
    b"#!",
    b"\xcf\xfa\xed\xfe",
    b"\xce\xfa\xed\xfe",
    b"\xfe\xed\xfa\xcf",
    b"\xfe\xed\xfa\xce",
    b"\xca\xfe\xba\xbe",
    b"\xbe\xba\xfe\xca",
    b"\xca\xfe\xba\xbf",
    b"\xbf\xba\xfe\xca",
    b"\x7fELF",
];

/// Ok(resolved path) or Err(errno: i32): ENOENT=2 when nothing on the search path stats, EACCES=13 when the first hit is
/// not an executable file (a directory included), or ENOEXEC=8 when it is executable but neither a script nor a binary.
pub fn locate(program: &str, cwd: &Path, path_env: Option<&str>) -> Result<PathBuf, i32> {
    let candidates: Vec<PathBuf> = if program.contains('/') {
        vec![cwd.join(program)]
    } else {
        path_env
            .unwrap_or(DEFAULT_PATH)
            .split(':')
            .map(|dir| {
                cwd.join(if dir.is_empty() { "." } else { dir })
                    .join(program)
            })
            .collect()
    };
    let mut denied = false;
    for candidate in candidates {
        // A path that does not stat (missing, dangling symlink, unreachable) is skipped, as os.path.exists does.
        let Ok(meta) = fs::metadata(&candidate) else {
            continue;
        };
        if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
            return check_magic(candidate);
        }
        // An existing file without the X bit, or a directory: EACCES unless a later entry runs.
        denied = true;
    }
    Err(if denied {
        Errno::ACCESS.raw_os_error()
    } else {
        Errno::NOENT.raw_os_error()
    })
}

/// Accepts a file whose first bytes name an executable format, else ENOEXEC. A file that cannot be read is accepted
/// unchecked: exec decides, as work.py does.
fn check_magic(candidate: PathBuf) -> Result<PathBuf, i32> {
    let mut head = Vec::with_capacity(4);
    let read = File::open(&candidate).and_then(|file| file.take(4).read_to_end(&mut head));
    match read {
        Ok(_) if !EXEC_MAGIC.iter().any(|magic| head.starts_with(magic)) => {
            Err(Errno::NOEXEC.raw_os_error())
        }
        _ => Ok(candidate),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    const EXEC: u32 = 0o755;
    const SCRIPT: &[u8] = b"#!/bin/sh\nexit 0\n";

    fn write(path: &Path, body: &[u8], mode: u32) {
        fs::write(path, body).expect("write file");
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set mode");
    }

    fn path_of(dirs: &[&Path]) -> String {
        dirs.iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(":")
    }

    #[test]
    fn finds_sh_on_the_given_path() {
        let cwd = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            locate("sh", cwd.path(), Some("/bin:/usr/bin")),
            Ok(PathBuf::from("/bin/sh"))
        );
    }

    #[test]
    fn default_path_is_searched_when_none() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let found = locate("sh", cwd.path(), None).expect("sh on the default path");
        assert!(
            found == Path::new("/usr/bin/sh") || found == Path::new("/bin/sh"),
            "found {found:?}"
        );
    }

    #[test]
    fn absolute_program_is_returned_unchanged() {
        let cwd = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            locate("/bin/sh", cwd.path(), Some("")),
            Ok(PathBuf::from("/bin/sh"))
        );
    }

    #[test]
    fn relative_script_with_shebang_resolves_against_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("script"), SCRIPT, EXEC);
        assert_eq!(
            locate("./script", dir.path(), None),
            Ok(dir.path().join("script"))
        );
    }

    #[test]
    fn every_binary_magic_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = dir.path().join("tool");
        for magic in EXEC_MAGIC {
            let mut body = magic.to_vec();
            body.extend_from_slice(b"\0padding");
            write(&tool, &body, EXEC);
            assert_eq!(
                locate("tool", dir.path(), Some(&path_of(&[dir.path()]))),
                Ok(tool.clone()),
                "{magic:?}"
            );
        }
    }

    #[test]
    fn fat_64_bit_magics_are_accepted_in_both_byte_orders() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = dir.path().join("tool");
        for magic in [b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca"] {
            let mut body = magic.to_vec();
            body.extend_from_slice(b"\0padding");
            write(&tool, &body, EXEC);
            assert_eq!(
                locate("tool", dir.path(), Some(&path_of(&[dir.path()]))),
                Ok(tool.clone()),
                "{magic:?}"
            );
        }
    }

    #[test]
    fn shebang_less_executable_is_enoexec() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("tool"), b"echo hi\n", EXEC);
        assert_eq!(
            locate("tool", dir.path(), Some(&path_of(&[dir.path()]))),
            Err(8)
        );
    }

    #[test]
    fn file_without_execute_bit_is_eacces() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("tool"), SCRIPT, 0o644);
        assert_eq!(
            locate("tool", dir.path(), Some(&path_of(&[dir.path()]))),
            Err(13)
        );
    }

    #[test]
    fn directory_on_path_is_eacces() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("tool")).expect("mkdir");
        assert_eq!(
            locate("tool", dir.path(), Some(&path_of(&[dir.path()]))),
            Err(13)
        );
    }

    #[test]
    fn missing_program_is_enoent() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            locate("no-such-tool", dir.path(), Some(&path_of(&[dir.path()]))),
            Err(2)
        );
    }

    #[test]
    fn dangling_symlink_counts_as_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        symlink("/no/such/target", dir.path().join("dangling")).expect("symlink");
        assert_eq!(
            locate("dangling", dir.path(), Some(&path_of(&[dir.path()]))),
            Err(2)
        );
    }

    #[test]
    fn empty_path_entry_resolves_in_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("tool"), SCRIPT, EXEC);
        assert_eq!(
            locate("tool", dir.path(), Some("")),
            Ok(dir.path().join("tool"))
        );
        assert_eq!(
            locate("tool", dir.path(), Some(":/no-such-dir")),
            Ok(dir.path().join("tool"))
        );
    }

    #[test]
    fn search_continues_past_a_non_executable_hit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir_all(&a).expect("mkdir a");
        fs::create_dir_all(&b).expect("mkdir b");
        write(&a.join("tool"), SCRIPT, 0o644);
        write(&b.join("tool"), SCRIPT, EXEC);
        assert_eq!(
            locate("tool", dir.path(), Some(&path_of(&[&a, &b]))),
            Ok(b.join("tool"))
        );
    }

    #[test]
    fn non_executable_hit_is_eacces_when_nothing_later_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir_all(&a).expect("mkdir a");
        fs::create_dir_all(&b).expect("mkdir b");
        write(&a.join("tool"), SCRIPT, 0o644);
        assert_eq!(
            locate("tool", dir.path(), Some(&path_of(&[&a, &b]))),
            Err(13)
        );
    }

    #[test]
    fn shebang_less_hit_stops_the_search() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir_all(&a).expect("mkdir a");
        fs::create_dir_all(&b).expect("mkdir b");
        write(&a.join("tool"), b"echo hi\n", EXEC);
        write(&b.join("tool"), SCRIPT, EXEC);
        assert_eq!(
            locate("tool", dir.path(), Some(&path_of(&[&a, &b]))),
            Err(8)
        );
    }

    #[test]
    fn slash_program_ignores_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        write(&dir.path().join("sub/tool"), SCRIPT, EXEC);
        assert_eq!(
            locate("sub/tool", dir.path(), Some("/no-such-dir")),
            Ok(dir.path().join("sub/tool"))
        );
    }

    #[test]
    fn symlink_is_returned_not_canonicalized() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("real"), SCRIPT, EXEC);
        symlink(dir.path().join("real"), dir.path().join("link")).expect("symlink");
        assert_eq!(
            locate("link", dir.path(), Some(&path_of(&[dir.path()]))),
            Ok(dir.path().join("link"))
        );
    }
}
