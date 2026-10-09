use super::*;

// ---------------------------------------------------------------------------------------------------------------------
// qos::wrap

const ARGS: [&str; 3] = ["-v", "two words", ""];

fn args() -> Vec<String> {
    ARGS.iter().map(|arg| (*arg).to_owned()).collect()
}

fn strs(argv: &[String]) -> Vec<&str> {
    argv.iter().map(String::as_str).collect()
}

/// The argv of a job that is not wrapped: the program, then its arguments.
fn bare_argv() -> Vec<&'static str> {
    std::iter::once("/bin/echo").chain(ARGS).collect()
}

/// `prefix` followed by the bare argv.
#[cfg(target_os = "macos")]
fn prefixed(prefix: &[&'static str]) -> Vec<&'static str> {
    prefix.iter().copied().chain(bare_argv()).collect()
}

#[test]
fn exclusive_jobs_run_unwrapped_at_every_qos() {
    for qos in [Qos::Utility, Qos::Default, Qos::Background] {
        let (argv, recorded) = wrap(qos, true, Path::new("/bin/echo"), &args());
        assert_eq!(recorded, "default", "exclusive job at {qos:?}");
        assert_eq!(strs(&argv), bare_argv(), "exclusive job at {qos:?}");
    }
}

#[test]
fn default_qos_runs_unwrapped_and_records_default() {
    let (argv, recorded) = wrap(Qos::Default, false, Path::new("/bin/echo"), &args());
    assert_eq!(recorded, "default");
    assert_eq!(strs(&argv), bare_argv());
}

#[test]
fn a_wrapped_job_keeps_its_program_and_arguments_as_the_final_argv_entries() {
    for qos in [Qos::Utility, Qos::Background] {
        let (argv, recorded) = wrap(qos, false, Path::new("/bin/echo"), &args());
        let argv = strs(&argv);
        if recorded == "default" {
            assert_eq!(argv, bare_argv(), "{qos:?} ran unwrapped");
        } else {
            assert_eq!(recorded, qos.as_str());
            assert!(argv.len() > bare_argv().len(), "{qos:?} has a wrapper");
            assert_eq!(
                &argv[argv.len() - bare_argv().len()..],
                bare_argv().as_slice()
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_utility_job_runs_under_taskpolicy_dash_c_utility_when_installed() {
    let (argv, recorded) = wrap(Qos::Utility, false, Path::new("/bin/echo"), &args());
    if Path::new("/usr/sbin/taskpolicy").is_file() {
        assert_eq!(recorded, "utility");
        assert_eq!(
            strs(&argv),
            prefixed(&["/usr/sbin/taskpolicy", "-c", "utility"])
        );
    } else {
        assert_eq!(recorded, "default");
        assert_eq!(strs(&argv), bare_argv());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_background_job_runs_under_taskpolicy_dash_c_background_when_installed() {
    let (argv, recorded) = wrap(Qos::Background, false, Path::new("/bin/echo"), &args());
    if Path::new("/usr/sbin/taskpolicy").is_file() {
        assert_eq!(recorded, "background");
        assert_eq!(
            strs(&argv),
            prefixed(&["/usr/sbin/taskpolicy", "-c", "background"])
        );
    } else {
        assert_eq!(recorded, "default");
        assert_eq!(strs(&argv), bare_argv());
    }
}
