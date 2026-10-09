use super::*;

// ---------------------------------------------------------------------------------------------------------------------
// locate: execvp emulation

const SCRIPT: &[u8] = b"#!/bin/sh\nexit 0\n";

fn file_with_mode(path: &Path, body: &[u8], mode: u32) {
    fs::write(path, body).expect("write file");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set mode");
}

fn on_path(dir: &Path) -> String {
    dir.display().to_string()
}

#[test]
fn locate_reports_enoent_for_a_program_missing_from_the_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        locate("no-such-tool", dir.path(), Some(&on_path(dir.path()))),
        Err(2)
    );
}

#[test]
fn locate_reports_enoent_for_an_absolute_path_that_does_not_exist() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        locate("/no/such/agora-tool", dir.path(), Some("/bin")),
        Err(2)
    );
}

#[test]
fn locate_reports_eacces_for_a_file_without_an_execute_bit() {
    let dir = tempfile::tempdir().expect("tempdir");
    file_with_mode(&dir.path().join("tool"), SCRIPT, 0o644);
    assert_eq!(
        locate("tool", dir.path(), Some(&on_path(dir.path()))),
        Err(13)
    );
}

#[test]
fn locate_reports_eacces_for_a_directory_named_like_the_program() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join("tool")).expect("mkdir");
    assert_eq!(
        locate("tool", dir.path(), Some(&on_path(dir.path()))),
        Err(13)
    );
}

#[test]
fn locate_reports_enoexec_for_an_executable_with_neither_shebang_nor_binary_magic() {
    let dir = tempfile::tempdir().expect("tempdir");
    file_with_mode(&dir.path().join("tool"), b"echo hi\n", 0o755);
    assert_eq!(
        locate("tool", dir.path(), Some(&on_path(dir.path()))),
        Err(8)
    );
}

#[test]
fn locate_finds_sh_through_the_given_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        locate("sh", dir.path(), Some("/bin:/usr/bin")),
        Ok(PathBuf::from("/bin/sh"))
    );
}

#[test]
fn locate_resolves_a_dot_slash_program_against_the_working_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    file_with_mode(&dir.path().join("x"), SCRIPT, 0o755);
    assert_eq!(
        locate("./x", dir.path(), Some("/no-such-dir")),
        Ok(dir.path().join("x"))
    );
}

#[test]
fn locate_treats_an_empty_path_entry_as_the_working_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    file_with_mode(&dir.path().join("tool"), SCRIPT, 0o755);
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
fn locate_without_a_path_searches_the_system_dirs_not_the_working_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    file_with_mode(&dir.path().join("agora-cwd-only-tool"), SCRIPT, 0o755);
    assert_eq!(locate("agora-cwd-only-tool", dir.path(), None), Err(2));
}
