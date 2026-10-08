//! QoS wrapping of non-exclusive jobs (D15): taskpolicy on macOS, nice/ionice on Linux.

use std::path::Path;

use agora_sched::settings::Qos;

const DEFAULT: &str = "default";

/// Returns (argv to exec, recorded qos string). `default` or exclusive → (resolved + args, "default").
///
/// The wrapper execs the program in place, so the pid, the process group and the signals stay the job's own.
/// The argv is a `Vec<String>`, so a `resolved` path that is not valid UTF-8 cannot be carried exactly: such a
/// path is never wrapped, and it is passed on through `to_string_lossy`. The lossy form may not name the file
/// that was located, so the exec can fail; callers that must run non-UTF-8 program paths need an `OsString` argv.
pub fn wrap(qos: Qos, exclusive: bool, resolved: &Path, args: &[String]) -> (Vec<String>, String) {
    let wrapper = if exclusive || qos == Qos::Default {
        None
    } else {
        resolved.to_str().and_then(|path| wrapped(qos, path, args))
    };
    wrapper.unwrap_or_else(|| {
        (
            command(Vec::new(), &resolved.to_string_lossy(), args),
            DEFAULT.to_owned(),
        )
    })
}

/// The argv `lead… program args…` in that order.
fn command(lead: impl IntoIterator<Item = String>, program: &str, args: &[String]) -> Vec<String> {
    lead.into_iter()
        .chain(std::iter::once(program.to_owned()))
        .chain(args.iter().cloned())
        .collect()
}

/// Whether `path` is a regular file (symlinks followed) with at least one execute bit set.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The first executable `name` in `dirs`, searched in order.
#[cfg(target_os = "linux")]
fn find_in(name: &str, dirs: &[&str]) -> Option<std::path::PathBuf> {
    dirs.iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| is_executable_file(candidate))
}

/// A helper tool from the standard system directories.
#[cfg(target_os = "linux")]
fn find_tool(name: &str) -> Option<std::path::PathBuf> {
    find_in(name, &["/usr/bin", "/bin"])
}

#[cfg(target_os = "macos")]
const TASKPOLICY: &str = "/usr/sbin/taskpolicy";

/// The wrapped argv for `qos` if this host can wrap it; `None` means the job runs unwrapped.
#[cfg(target_os = "macos")]
fn wrapped(qos: Qos, program: &str, args: &[String]) -> Option<(Vec<String>, String)> {
    let policy = Path::new(TASKPOLICY);
    if !is_executable_file(policy) {
        return None;
    }
    policy_wrapper(qos, policy, program, args)
}

/// `taskpolicy -c <qos> program args…`, recorded as the QoS name. The default QoS has no wrapper.
#[cfg(target_os = "macos")]
fn policy_wrapper(
    qos: Qos,
    policy: &Path,
    program: &str,
    args: &[String],
) -> Option<(Vec<String>, String)> {
    if qos == Qos::Default {
        return None;
    }
    let lead = vec![
        policy.to_string_lossy().into_owned(),
        "-c".to_owned(),
        qos.as_str().to_owned(),
    ];
    Some((command(lead, program, args), qos.as_str().to_owned()))
}

/// The wrapped argv for `qos` if `nice` is found; `None` means the job runs unwrapped.
#[cfg(target_os = "linux")]
fn wrapped(qos: Qos, program: &str, args: &[String]) -> Option<(Vec<String>, String)> {
    let nice = find_tool("nice")?;
    let ionice = find_tool("ionice");
    nice_wrapper(qos, &nice, ionice.as_deref(), program, args)
}

/// `nice -n 10` for utility; `nice -n 19` plus `ionice -c 3` (when found) for background.
#[cfg(target_os = "linux")]
fn nice_wrapper(
    qos: Qos,
    nice: &Path,
    ionice: Option<&Path>,
    program: &str,
    args: &[String],
) -> Option<(Vec<String>, String)> {
    let mut lead: Vec<String> = vec![nice.to_string_lossy().into_owned(), "-n".to_owned()];
    match qos {
        Qos::Utility => lead.push("10".to_owned()),
        Qos::Background => {
            lead.push("19".to_owned());
            if let Some(ionice) = ionice {
                lead.extend([
                    ionice.to_string_lossy().into_owned(),
                    "-c".to_owned(),
                    "3".to_owned(),
                ]);
            }
        }
        Qos::Default => return None,
    }
    Some((command(lead, program, args), qos.as_str().to_owned()))
}

/// Other systems have no QoS wrapper; the job runs at the default QoS.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn wrapped(_qos: Qos, _program: &str, _args: &[String]) -> Option<(Vec<String>, String)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARGS: [&str; 4] = ["-a", "two words", "--flag=x", ""];

    fn args() -> Vec<String> {
        ARGS.iter().map(|arg| arg.to_string()).collect()
    }

    /// The expected argv: `lead`, then the program, then ARGS in order.
    fn expected<'a>(lead: &[&'a str], program: &'a str) -> Vec<&'a str> {
        lead.iter()
            .copied()
            .chain(std::iter::once(program))
            .chain(ARGS)
            .collect()
    }

    fn strs(argv: &[String]) -> Vec<&str> {
        argv.iter().map(String::as_str).collect()
    }

    #[test]
    fn exclusive_job_has_no_wrapper_at_any_qos() {
        for qos in [Qos::Utility, Qos::Background, Qos::Default] {
            let (argv, recorded) = wrap(qos, true, Path::new("/bin/echo"), &args());
            assert_eq!(recorded, "default", "qos {qos:?}");
            assert_eq!(strs(&argv), expected(&[], "/bin/echo"), "qos {qos:?}");
        }
    }

    #[test]
    fn default_qos_has_no_wrapper() {
        let (argv, recorded) = wrap(Qos::Default, false, Path::new("/bin/echo"), &args());
        assert_eq!(recorded, "default");
        assert_eq!(strs(&argv), expected(&[], "/bin/echo"));
    }

    #[test]
    fn program_without_args_is_argv_of_one() {
        let (argv, recorded) = wrap(Qos::Default, false, Path::new("/bin/true"), &[]);
        assert_eq!(recorded, "default");
        assert_eq!(strs(&argv), vec!["/bin/true"]);
    }

    #[test]
    fn wrapped_argv_keeps_program_and_args_in_order() {
        for qos in [Qos::Utility, Qos::Background] {
            let (argv, recorded) = wrap(qos, false, Path::new("/bin/echo"), &args());
            assert!(
                recorded == "default" || recorded == qos.as_str(),
                "qos {qos:?} recorded {recorded}"
            );
            let argv = strs(&argv);
            let tail = &argv[argv.len() - (ARGS.len() + 1)..];
            assert_eq!(tail, expected(&[], "/bin/echo").as_slice(), "qos {qos:?}");
        }
    }

    #[test]
    fn command_places_lead_then_program_then_args() {
        let lead = vec!["nice".to_owned(), "-n".to_owned(), "10".to_owned()];
        let argv = command(lead, "/bin/x", &["a".to_owned(), "b".to_owned()]);
        assert_eq!(strs(&argv), vec!["nice", "-n", "10", "/bin/x", "a", "b"]);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_program_is_not_wrapped_and_passed_lossily() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(OsStr::from_bytes(b"/tmp/bad\xff"));
        let (argv, recorded) = wrap(Qos::Utility, false, path, &args());
        assert_eq!(recorded, "default");
        assert_eq!(argv.len(), ARGS.len() + 1);
        assert_eq!(argv[0], "/tmp/bad\u{FFFD}");
        assert_eq!(strs(&argv[1..]), ARGS.to_vec());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[cfg(unix)]
    #[test]
    fn executable_file_needs_a_regular_file_with_an_execute_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let exec = dir.path().join("exec");
        let plain = dir.path().join("plain");
        std::fs::write(&exec, b"#!/bin/sh\n").expect("write");
        std::fs::write(&plain, b"data").expect("write");
        std::fs::set_permissions(&exec, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(is_executable_file(&exec));
        assert!(!is_executable_file(&plain));
        assert!(
            !is_executable_file(dir.path()),
            "a directory is not a program"
        );
        assert!(!is_executable_file(&dir.path().join("missing")));
    }

    #[cfg(target_os = "linux")]
    #[cfg(unix)]
    #[test]
    fn find_in_takes_the_first_executable_match_in_order() {
        use std::os::unix::fs::PermissionsExt;
        let first = tempfile::tempdir().expect("tempdir");
        let second = tempfile::tempdir().expect("tempdir");
        let first_nice = first.path().join("nice");
        let second_nice = second.path().join("nice");
        std::fs::write(&first_nice, b"data").expect("write");
        std::fs::write(&second_nice, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&first_nice, std::fs::Permissions::from_mode(0o644))
            .expect("chmod");
        std::fs::set_permissions(&second_nice, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        let first_dir = first.path().to_str().expect("utf8 tempdir");
        let second_dir = second.path().to_str().expect("utf8 tempdir");
        assert_eq!(
            find_in("nice", &[first_dir, second_dir]),
            Some(second_nice.clone())
        );
        assert_eq!(find_in("nice", &[second_dir, first_dir]), Some(second_nice));
        assert_eq!(find_in("missing", &[first_dir, second_dir]), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_utility_wrapper_uses_taskpolicy_dash_c() {
        let policy = Path::new("/opt/fake/taskpolicy");
        let (argv, recorded) =
            policy_wrapper(Qos::Utility, policy, "/bin/echo", &args()).expect("wrapped");
        assert_eq!(recorded, "utility");
        assert_eq!(
            strs(&argv),
            expected(&["/opt/fake/taskpolicy", "-c", "utility"], "/bin/echo")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_background_wrapper_uses_taskpolicy_background() {
        let policy = Path::new("/opt/fake/taskpolicy");
        let (argv, recorded) =
            policy_wrapper(Qos::Background, policy, "/bin/echo", &args()).expect("wrapped");
        assert_eq!(recorded, "background");
        assert_eq!(
            strs(&argv),
            expected(&["/opt/fake/taskpolicy", "-c", "background"], "/bin/echo")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_default_qos_has_no_policy_wrapper() {
        assert!(
            policy_wrapper(
                Qos::Default,
                Path::new("/usr/sbin/taskpolicy"),
                "/bin/echo",
                &args()
            )
            .is_none()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_utility_wraps_with_taskpolicy_exactly_when_it_is_executable() {
        let present = is_executable_file(Path::new(TASKPOLICY));
        let (argv, recorded) = wrap(Qos::Utility, false, Path::new("/bin/echo"), &args());
        if present {
            assert_eq!(recorded, "utility");
            assert_eq!(
                strs(&argv),
                expected(&[TASKPOLICY, "-c", "utility"], "/bin/echo")
            );
        } else {
            assert_eq!(recorded, "default");
            assert_eq!(strs(&argv), expected(&[], "/bin/echo"));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_utility_wrapper_uses_nice_10() {
        let nice = Path::new("/usr/bin/nice");
        let (argv, recorded) =
            nice_wrapper(Qos::Utility, nice, None, "/bin/echo", &args()).expect("wrapped");
        assert_eq!(recorded, "utility");
        assert_eq!(
            strs(&argv),
            expected(&["/usr/bin/nice", "-n", "10"], "/bin/echo")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_background_wrapper_uses_nice_19_and_ionice_idle_class() {
        let nice = Path::new("/usr/bin/nice");
        let ionice = Path::new("/usr/bin/ionice");
        let (argv, recorded) =
            nice_wrapper(Qos::Background, nice, Some(ionice), "/bin/echo", &args())
                .expect("wrapped");
        assert_eq!(recorded, "background");
        assert_eq!(
            strs(&argv),
            expected(
                &["/usr/bin/nice", "-n", "19", "/usr/bin/ionice", "-c", "3"],
                "/bin/echo"
            )
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_background_without_ionice_uses_nice_19_alone() {
        let nice = Path::new("/usr/bin/nice");
        let (argv, recorded) =
            nice_wrapper(Qos::Background, nice, None, "/bin/echo", &args()).expect("wrapped");
        assert_eq!(recorded, "background");
        assert_eq!(
            strs(&argv),
            expected(&["/usr/bin/nice", "-n", "19"], "/bin/echo")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_default_qos_has_no_nice_wrapper() {
        let nice = Path::new("/usr/bin/nice");
        assert!(nice_wrapper(Qos::Default, nice, None, "/bin/echo", &args()).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_wrap_uses_nice_exactly_when_nice_is_found() {
        let nice = find_tool("nice");
        let (argv, recorded) = wrap(Qos::Utility, false, Path::new("/bin/echo"), &args());
        match nice {
            Some(nice) => {
                let nice = nice.to_string_lossy().into_owned();
                assert_eq!(recorded, "utility");
                assert_eq!(
                    strs(&argv),
                    expected(&[nice.as_str(), "-n", "10"], "/bin/echo")
                );
            }
            None => {
                assert_eq!(recorded, "default");
                assert_eq!(strs(&argv), expected(&[], "/bin/echo"));
            }
        }
    }
}
