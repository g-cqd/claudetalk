use super::*;
use crate::install::safe_write::{BACKUP_TAG, KEEP_BACKUPS};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn backups_in(dir: &Path, name: &str) -> Vec<String> {
    let prefix = format!("{name}{BACKUP_TAG}");
    names_in(dir)
        .into_iter()
        .filter(|n| n.starts_with(&prefix))
        .collect()
}

#[test]
fn read_missing_file_is_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    let read = read_text(&dir.path().join("config.toml")).expect("read");
    assert_eq!(read, None);
}

#[test]
fn read_returns_the_file_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("AGENTS.md");
    fs::write(&path, "héllo\n").expect("write");
    assert_eq!(read_text(&path).expect("read"), Some("héllo\n".to_owned()));
}

#[test]
fn read_symlink_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("real.md");
    let link = dir.path().join("link.md");
    fs::write(&real, "x\n").expect("write");
    symlink(&real, &link).expect("symlink");
    let msg = read_text(&link).expect_err("symlink").to_string();
    assert!(msg.contains("refusing to read through symlink"), "{msg}");
    assert!(msg.contains(&link.display().to_string()), "{msg}");
}

#[test]
fn read_too_large_file_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("big.md");
    let size = usize::try_from(MAX_TEXT_BYTES + 1).expect("fits in usize");
    fs::write(&path, vec![b'a'; size]).expect("write");
    let msg = read_text(&path).expect_err("too large").to_string();
    assert!(msg.contains("is larger than 4000000 bytes"), "{msg}");
    assert!(msg.contains(&path.display().to_string()), "{msg}");
}

#[test]
fn read_non_utf8_file_is_refused_naming_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("binary.toml");
    fs::write(&path, [0xff_u8, 0xfe, b'a']).expect("write");
    let msg = read_text(&path).expect_err("non-utf8").to_string();
    assert!(msg.contains("is not UTF-8 text"), "{msg}");
    assert!(msg.contains(&path.display().to_string()), "{msg}");
}

#[test]
fn write_new_file_creates_parents_with_mode_0644() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested").join("config.toml");
    let result = write_text(&path, "a = 1\n", 0o644, false).expect("write");
    assert_eq!(result, WriteResult::Wrote { backup: None });
    assert_eq!(mode_of(&path), 0o644);
    assert_eq!(fs::read_to_string(&path).expect("read"), "a = 1\n");
}

#[test]
fn write_identical_text_is_unchanged_without_backup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("AGENTS.md");
    write_text(&path, "same\n", 0o644, false).expect("first write");
    let again = write_text(&path, "same\n", 0o644, false).expect("second write");
    assert_eq!(again, WriteResult::Unchanged);
    assert!(backups_in(dir.path(), "AGENTS.md").is_empty());
}

#[test]
fn write_keeps_existing_mode_and_backs_up_old_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(&path, "old = true\n").expect("write");
    set_mode(&path, 0o640);
    let result = write_text(&path, "new = true\n", 0o644, false).expect("write");
    let WriteResult::Wrote {
        backup: Some(backup),
    } = result
    else {
        panic!("expected a backup, got {result:?}");
    };
    assert_eq!(fs::read_to_string(&path).expect("read"), "new = true\n");
    assert_eq!(mode_of(&path), 0o640);
    assert_eq!(fs::read(&backup).expect("backup"), b"old = true\n");
    assert_eq!(mode_of(&backup), 0o640);
}

#[test]
fn dry_run_writes_nothing_and_shows_the_line_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    fs::write(&path, "a\nb\nc\n").expect("write");
    let result = write_text(&path, "a\nB\nc\n", 0o644, true).expect("dry run");
    assert_eq!(
        result,
        WriteResult::DryRun {
            diff: vec!["- b".to_owned(), "+ B".to_owned()],
        }
    );
    assert_eq!(fs::read_to_string(&path).expect("read"), "a\nb\nc\n");
    assert_eq!(names_in(dir.path()), vec!["config.toml".to_owned()]);
}

#[test]
fn dry_run_for_a_new_file_creates_nothing_and_lists_every_line_as_added() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("missing").join("rule.mdc");
    let result = write_text(&path, "one\ntwo\n", 0o644, true).expect("dry run");
    assert_eq!(
        result,
        WriteResult::DryRun {
            diff: vec!["+ one".to_owned(), "+ two".to_owned()],
        }
    );
    assert!(!path.exists());
    assert!(!dir.path().join("missing").exists());
}

#[test]
fn dry_run_of_identical_text_is_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("AGENTS.md");
    fs::write(&path, "same\n").expect("write");
    let result = write_text(&path, "same\n", 0o644, true).expect("dry run");
    assert_eq!(result, WriteResult::Unchanged);
}

#[test]
fn symlink_is_refused_for_write_and_dry_run_and_target_is_untouched() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("real.toml");
    let link = dir.path().join("config.toml");
    fs::write(&real, "a = 1\n").expect("write");
    symlink(&real, &link).expect("symlink");
    for dry_run in [false, true] {
        let msg = write_text(&link, "a = 2\n", 0o644, dry_run)
            .expect_err("symlink refused")
            .to_string();
        assert!(msg.contains("refusing to write through symlink"), "{msg}");
    }
    assert_eq!(fs::read_to_string(&real).expect("read"), "a = 1\n");
    assert!(backups_in(dir.path(), "config.toml").is_empty());
}

#[test]
fn line_diff_of_identical_text_is_empty() {
    assert!(line_diff("a\nb\n", "a\nb\n").is_empty());
}

#[test]
fn line_diff_shows_only_the_differing_middle() {
    assert_eq!(
        line_diff("a\nb\nc\n", "a\nx\ny\nc\n"),
        vec!["- b", "+ x", "+ y"]
    );
}

#[test]
fn line_diff_lists_appended_and_removed_lines() {
    assert_eq!(line_diff("a\nb\n", "a\nb\nc\n"), vec!["+ c"]);
    assert_eq!(line_diff("a\nb\nc\n", "a\nc\n"), vec!["- b"]);
}

#[test]
fn line_diff_from_empty_old_text_is_all_added() {
    assert_eq!(line_diff("", "x\ny\n"), vec!["+ x", "+ y"]);
}

#[test]
fn line_diff_reports_a_trailing_newline_change() {
    assert_eq!(line_diff("a\n", "a"), vec!["- a", "+ a"]);
}

#[test]
fn line_diff_is_capped_at_200_lines_with_a_count() {
    let old: String = (0..300).map(|i| format!("old {i}\n")).collect();
    let new: String = (0..300).map(|i| format!("new {i}\n")).collect();
    let diff = line_diff(&old, &new);
    assert_eq!(diff.len(), 200);
    assert_eq!(diff[0], "- old 0");
    assert_eq!(diff[199], "… 401 more lines");
}

#[test]
fn remove_missing_file_is_absent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("rule.mdc");
    let act = remove_file(&path, false).expect("remove");
    assert_eq!(act.result, "absent");
    assert_eq!(act.path, path.display().to_string());
    assert!(names_in(dir.path()).is_empty());
}

#[test]
fn remove_backs_up_then_deletes_keeping_the_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("rule.mdc");
    fs::write(&path, "bye\n").expect("write");
    set_mode(&path, 0o640);
    let act = remove_file(&path, false).expect("remove");
    assert_eq!(act.result, "removed");
    assert!(!path.exists());
    let backup = PathBuf::from(
        act.detail
            .strip_prefix("backup ")
            .expect("detail names the backup"),
    );
    assert_eq!(fs::read(&backup).expect("backup"), b"bye\n");
    assert_eq!(mode_of(&backup), 0o640);
}

#[test]
fn remove_dry_run_touches_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("rule.mdc");
    fs::write(&path, "keep\n").expect("write");
    let act = remove_file(&path, true).expect("dry run");
    assert_eq!(act.result, "dry_run");
    assert_eq!(act.detail, "would remove");
    assert_eq!(fs::read(&path).expect("read"), b"keep\n");
    assert!(backups_in(dir.path(), "rule.mdc").is_empty());
}

#[test]
fn remove_refuses_a_symlink_and_keeps_the_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("real.mdc");
    let link = dir.path().join("rule.mdc");
    fs::write(&real, "x\n").expect("write");
    symlink(&real, &link).expect("symlink");
    let msg = remove_file(&link, false).expect_err("symlink").to_string();
    assert!(msg.contains("refusing to remove symlink"), "{msg}");
    assert_eq!(fs::read(&real).expect("read"), b"x\n");
    assert!(
        fs::symlink_metadata(&link)
            .expect("lstat")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn remove_keeps_only_the_newest_three_backups() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("rule.mdc");
    for ms in 1..=3 {
        fs::write(
            dir.path().join(format!("rule.mdc{BACKUP_TAG}{ms}")),
            "old\n",
        )
        .expect("seed");
    }
    fs::write(&path, "bye\n").expect("write");
    remove_file(&path, false).expect("remove");
    let backups = backups_in(dir.path(), "rule.mdc");
    assert_eq!(backups.len(), KEEP_BACKUPS);
    assert!(!dir.path().join(format!("rule.mdc{BACKUP_TAG}1")).exists());
}
