//! Exit codes of the work commands and job statuses.

use crate::model::TaskState;

pub const EXIT_REFUSED: i32 = 1;
pub const EXIT_INPUT: i32 = 2;
pub const EXIT_TIMEOUT: i32 = 124;
pub const EXIT_NOT_EXECUTABLE: i32 = 126;
pub const EXIT_NOT_FOUND: i32 = 127;
pub const EXIT_INTERRUPTED: i32 = 130;

const EACCES: i32 = 13;
const EPERM: i32 = 1;
const ENOEXEC: i32 = 8;
const EISDIR: i32 = 21;

/// code -> code; killed by signal N -> 128+N.
pub fn exit_status(code: Option<i32>, signal: Option<i32>) -> Option<i32> {
    match (code, signal) {
        (Some(code), _) => Some(code),
        (None, Some(signal)) => Some(128_i32.saturating_add(signal)),
        (None, None) => None,
    }
}

/// EACCES, EPERM, ENOEXEC, EISDIR -> 126, anything else -> 127 (pass raw errno; use libc-free constants from rustix
/// in agora-run, here plain i32 values: EACCES=13, EPERM=1, ENOEXEC=8, EISDIR=21 on both macOS and Linux).
pub fn spawn_error_status(errno: i32) -> i32 {
    match errno {
        EACCES | EPERM | ENOEXEC | EISDIR => EXIT_NOT_EXECUTABLE,
        _ => EXIT_NOT_FOUND,
    }
}

/// done -> status or 0; other final states -> status or 1.
pub fn task_exit_status(state: TaskState, status: Option<i32>) -> i32 {
    match state {
        TaskState::Done => status.unwrap_or(0),
        _ => status.unwrap_or(EXIT_REFUSED),
    }
}

/// run/wait exit: timed_out -> 124; any=true -> status of the first task (by ended_ms, then given order) that ended;
/// else the exit status of the first task, in the order given, whose state is not done; else 0.
pub fn wait_exit(
    rows: &[(TaskState, Option<i32>, Option<i64>)],
    timed_out: bool,
    any: bool,
) -> i32 {
    if timed_out {
        return EXIT_TIMEOUT;
    }
    if any {
        // Only final rows have ended. Dated rows sort before undated ones; ties fall back to the given order.
        return rows
            .iter()
            .enumerate()
            .filter(|(_, (state, _, _))| state.is_final())
            .min_by_key(|(index, (_, _, ended))| (ended.is_none(), ended.unwrap_or(0), *index))
            .map_or(0, |(_, (state, status, _))| {
                task_exit_status(*state, *status)
            });
    }
    rows.iter()
        .find(|(state, _, _)| *state != TaskState::Done)
        .map_or(0, |(state, status, _)| task_exit_status(*state, *status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TaskState::{Cancelled, Claimed, Done, Failed, Queued, Running};

    #[test]
    fn exit_status_keeps_a_recorded_code() {
        assert_eq!(exit_status(Some(0), None), Some(0));
        assert_eq!(exit_status(Some(3), None), Some(3));
        assert_eq!(exit_status(Some(127), None), Some(127));
    }

    #[test]
    fn exit_status_code_wins_over_signal() {
        assert_eq!(exit_status(Some(2), Some(9)), Some(2));
    }

    #[test]
    fn exit_status_signal_is_128_plus_number() {
        assert_eq!(exit_status(None, Some(9)), Some(137));
        assert_eq!(exit_status(None, Some(15)), Some(143));
        assert_eq!(exit_status(None, Some(1)), Some(129));
    }

    #[test]
    fn exit_status_without_code_or_signal_is_none() {
        assert_eq!(exit_status(None, None), None);
    }

    #[test]
    fn exit_status_signal_does_not_overflow() {
        assert_eq!(exit_status(None, Some(i32::MAX)), Some(i32::MAX));
    }

    #[test]
    fn spawn_error_not_executable_errnos_map_to_126() {
        for errno in [13, 1, 8, 21] {
            assert_eq!(spawn_error_status(errno), 126, "errno {errno}");
        }
    }

    #[test]
    fn spawn_error_other_errnos_map_to_127() {
        // ENOENT, ENOMEM, EINVAL and an unknown zero.
        for errno in [2, 12, 22, 0] {
            assert_eq!(spawn_error_status(errno), 127, "errno {errno}");
        }
    }

    #[test]
    fn task_done_without_status_exits_zero() {
        assert_eq!(task_exit_status(Done, None), 0);
    }

    #[test]
    fn task_done_returns_its_recorded_status() {
        assert_eq!(task_exit_status(Done, Some(0)), 0);
        assert_eq!(task_exit_status(Done, Some(5)), 5);
    }

    #[test]
    fn task_failed_or_cancelled_without_status_exits_one() {
        assert_eq!(task_exit_status(Failed, None), 1);
        assert_eq!(task_exit_status(Cancelled, None), 1);
    }

    #[test]
    fn task_failed_or_cancelled_returns_recorded_status() {
        assert_eq!(task_exit_status(Failed, Some(137)), 137);
        assert_eq!(task_exit_status(Cancelled, Some(143)), 143);
    }

    #[test]
    fn task_non_final_states_without_status_exit_one() {
        for state in [Queued, Claimed, Running] {
            assert_eq!(task_exit_status(state, None), 1, "{state:?}");
        }
    }

    #[test]
    fn wait_timeout_is_124_even_when_every_task_is_done() {
        let rows = [(Done, Some(0), Some(1))];
        assert_eq!(wait_exit(&rows, true, false), 124);
        assert_eq!(wait_exit(&rows, true, true), 124);
    }

    #[test]
    fn wait_all_done_exits_zero() {
        let rows = [(Done, None, Some(1)), (Done, Some(0), Some(2))];
        assert_eq!(wait_exit(&rows, false, false), 0);
    }

    #[test]
    fn wait_all_reports_first_non_done_row_in_given_order() {
        let rows = [
            (Done, None, None),
            (Failed, Some(2), None),
            (Cancelled, Some(143), None),
        ];
        assert_eq!(wait_exit(&rows, false, false), 2);

        let reversed = [(Cancelled, Some(143), None), (Failed, Some(2), None)];
        assert_eq!(wait_exit(&reversed, false, false), 143);
    }

    #[test]
    fn wait_all_skips_done_rows_even_with_a_status() {
        let rows = [(Done, Some(3), Some(1)), (Cancelled, None, Some(2))];
        assert_eq!(wait_exit(&rows, false, false), 1);
    }

    #[test]
    fn wait_all_counts_non_final_rows_as_not_done() {
        let rows = [(Done, None, None), (Running, None, None)];
        assert_eq!(wait_exit(&rows, false, false), 1);
    }

    #[test]
    fn wait_any_picks_smallest_ended_not_given_order() {
        // Done is listed first but ended later than the failed row.
        let rows = [(Done, None, Some(300)), (Failed, Some(2), Some(100))];
        assert_eq!(wait_exit(&rows, false, true), 2);
    }

    #[test]
    fn wait_any_ties_go_to_the_earliest_given_row() {
        let rows = [(Failed, Some(7), Some(100)), (Failed, Some(9), Some(100))];
        assert_eq!(wait_exit(&rows, false, true), 7);
    }

    #[test]
    fn wait_any_final_rows_without_ended_come_after_dated_ones() {
        let dated_later = [(Failed, Some(4), None), (Failed, Some(6), Some(500))];
        assert_eq!(wait_exit(&dated_later, false, true), 6);

        // Neither row is dated: the given order decides.
        let undated = [(Failed, Some(3), None), (Done, None, None)];
        assert_eq!(wait_exit(&undated, false, true), 3);
    }

    #[test]
    fn wait_any_ignores_rows_that_have_not_ended() {
        let rows = [
            (Running, None, Some(1)),
            (Queued, None, None),
            (Failed, Some(4), Some(900)),
        ];
        assert_eq!(wait_exit(&rows, false, true), 4);
    }

    #[test]
    fn wait_any_reports_signal_status_of_first_ended() {
        let rows = [
            (Cancelled, Some(143), Some(10)),
            (Failed, Some(137), Some(20)),
        ];
        assert_eq!(wait_exit(&rows, false, true), 143);
    }

    #[test]
    fn wait_any_without_a_final_row_exits_zero() {
        let rows = [(Running, None, None)];
        assert_eq!(wait_exit(&rows, false, true), 0);
    }

    #[test]
    fn wait_empty_rows_exit_zero() {
        assert_eq!(wait_exit(&[], false, false), 0);
        assert_eq!(wait_exit(&[], false, true), 0);
    }
}
