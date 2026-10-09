//! Predictions (ref-admission section 2 and 15 F) over history built through the signatures.

use super::{OTHER, PLACE, words};
use agora_sched::predict::{History, Level, Prediction, Run, percentile, predict};
use agora_sched::signature::{Signatures, signatures};

// Predictions (ref-admission section 2 and 15 F). History is built through the signatures, the way the store
// pre-fetches it: per level, the newest runs whose signature at that level equals the query's.

/// A finished run of `argv` in `place`.
#[derive(Clone)]
struct Past {
    argv: &'static [&'static str],
    place: &'static str,
    run: Run,
}

/// A run of `secs` seconds from time 0 that used `cores` CPUs on average, with the given peak.
fn run(secs: i64, cores: f64, peak: Option<u64>) -> Run {
    Run {
        started_ms: 0,
        ended_ms: secs * 1000,
        cpu_seconds: secs as f64 * cores,
        rss_peak_kib: peak,
    }
}

fn seconds(run: &Run) -> f64 {
    (run.ended_ms - run.started_ms) as f64 / 1000.0
}

fn key(sig: &Signatures, level: usize) -> &str {
    match level {
        0 => &sig.full,
        1 => &sig.head,
        2 => &sig.prog,
        _ => &sig.tool,
    }
}

/// The history a prediction for `argv` in `place` reads. `runs` are in the order they ended, oldest first. Per level
/// the history holds the newest five runs whose signature at that level equals the query's; the durations of every
/// run are kept for the recent-run median.
fn history_for(runs: &[Past], argv: &[&str], place: &str) -> History {
    let query = signatures(&words(argv), place);
    let mut history = History::default();
    for past in runs.iter().rev() {
        let theirs = signatures(&words(past.argv), past.place);
        history.recent_durations_s.push(seconds(&past.run));
        for (level, bucket) in history.by_level.iter_mut().enumerate() {
            if key(&theirs, level) == key(&query, level) && bucket.len() < 5 {
                bucket.push(past.run.clone());
            }
        }
    }
    history
}

/// A history with runs only at the full level, newest first.
fn full_only(runs: Vec<Run>) -> History {
    History {
        by_level: [runs, Vec::new(), Vec::new(), Vec::new()],
        ..History::default()
    }
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn no_history_predicts_the_declared_weight_for_a_thirty_second_run() {
    assert_eq!(
        predict(&History::default(), 3.0, 8.0),
        Prediction {
            cores: 3.0,
            rss_kib: None,
            rss_p90_kib: None,
            duration_s: 30.0,
            duration_p90_s: None,
            level: Level::None,
            runs: 0,
        }
    );
    assert_eq!(
        predict(&History::default(), 3.0, 8.0).basis(),
        "no history: the declared weight"
    );
}

#[test]
fn without_history_the_duration_is_the_median_of_recent_runs_of_any_command() {
    let h = History {
        recent_durations_s: vec![90.0, 10.0, 20.0],
        ..History::default()
    };
    let p = predict(&h, 3.0, 8.0);
    assert_eq!(p.level, Level::None);
    assert_eq!(p.duration_s, 20.0);
    assert_eq!(p.duration_p90_s, None);
}

#[test]
fn the_full_level_reads_only_the_five_newest_runs() {
    // Newest first; the sixth, oldest run would move the median, the peak and the p90 if it were read.
    let h = full_only(vec![
        run(60, 1.0, Some(400_000)),
        run(80, 1.5, Some(900_000)),
        run(50, 0.8, Some(300_000)),
        run(70, 3.0, Some(500_000)),
        run(65, 1.2, Some(450_000)),
        run(500, 7.0, Some(9_000_000)),
    ]);
    let p = predict(&h, 7.5, 8.0);
    assert_eq!(p.level, Level::Full);
    assert_close(p.cores, 1.2);
    assert_eq!(p.rss_kib, Some(900_000));
    assert_eq!(p.rss_p90_kib, Some(900_000));
    assert_close(p.duration_s, 65.0);
    assert_eq!(p.duration_p90_s, Some(80.0));
    assert_eq!(p.runs, 5);
    assert_eq!(p.basis(), "5 runs of the same command");
}

#[test]
fn cores_are_capped_at_the_declared_weight() {
    let p = predict(&full_only(vec![run(10, 6.0, None)]), 2.0, 8.0);
    assert_eq!(p.level, Level::Full);
    assert_close(p.cores, 2.0);
}

#[test]
fn cores_are_floored_at_a_tenth_of_a_core() {
    let p = predict(&full_only(vec![run(10, 0.0, None)]), 8.0, 8.0);
    assert_close(p.cores, 0.1);
}

#[test]
fn a_run_that_ends_no_later_than_it_starts_lasts_a_twentieth_of_a_second() {
    let zero = Run {
        started_ms: 5_000,
        ended_ms: 5_000,
        cpu_seconds: 0.05,
        rss_peak_kib: None,
    };
    let backwards = Run {
        started_ms: 5_000,
        ended_ms: 1_000,
        cpu_seconds: 0.01,
        rss_peak_kib: None,
    };
    let p = predict(&full_only(vec![zero]), 8.0, 8.0);
    assert_close(p.duration_s, 0.05);
    assert_close(p.cores, 1.0);
    let q = predict(&full_only(vec![backwards]), 8.0, 8.0);
    assert_close(q.duration_s, 0.05);
    assert_close(q.cores, 0.2);
}

#[test]
fn timestamps_at_the_ends_of_the_i64_range_do_not_overflow() {
    let span = Run {
        started_ms: i64::MIN,
        ended_ms: i64::MAX,
        cpu_seconds: 1.0,
        rss_peak_kib: None,
    };
    let p = predict(&full_only(vec![span]), 8.0, 8.0);
    assert_eq!(p.duration_s, i64::MAX as f64 / 1000.0);
    assert_close(p.cores, 0.1);
}

#[test]
fn an_even_count_of_runs_takes_the_mean_of_the_middle_two_cores() {
    let p = predict(
        &full_only(vec![run(10, 2.0, None), run(10, 1.0, None)]),
        8.0,
        8.0,
    );
    assert_eq!(p.level, Level::Full);
    assert_close(p.cores, 1.5);
}

#[test]
fn peaks_missing_from_some_runs_are_ignored_and_all_missing_gives_none() {
    let mixed = predict(
        &full_only(vec![
            run(10, 1.0, None),
            run(10, 1.0, Some(300)),
            run(10, 1.0, Some(100)),
        ]),
        8.0,
        8.0,
    );
    assert_eq!((mixed.rss_kib, mixed.rss_p90_kib), (Some(300), Some(300)));
    let none = predict(&full_only(vec![run(10, 1.0, None)]), 8.0, 8.0);
    assert_eq!((none.rss_kib, none.rss_p90_kib), (None, None));
}

#[test]
fn percentile_is_nearest_rank_so_up_to_five_values_give_the_maximum() {
    for n in 1..=5 {
        let xs: Vec<f64> = (1..=n).map(f64::from).collect();
        assert_eq!(percentile(&xs, 0.9), Some(f64::from(n)), "n = {n}");
    }
    let ten: Vec<f64> = (1..=10).map(f64::from).collect();
    assert_eq!(percentile(&ten, 0.9), Some(9.0));
}

#[test]
fn a_weight_below_a_tenth_of_a_core_is_not_raised_to_the_floor() {
    assert_close(predict(&History::default(), 0.05, 8.0).cores, 0.05);
    let busy = full_only(vec![run(10, 10.0, None)]);
    assert_close(predict(&busy, 0.05, 8.0).cores, 0.05);
}

#[test]
fn a_program_is_predicted_from_its_products_as_their_heads_fill() {
    let app = Past {
        argv: &["swift", "build", "--product", "App"],
        place: PLACE,
        run: run(100, 4.0, None),
    };
    let kit = Past {
        argv: &["swift", "build", "--product", "Kit"],
        place: PLACE,
        run: run(50, 2.0, None),
    };
    let verbose = Past {
        argv: &["swift", "build", "-v"],
        place: PLACE,
        run: run(30, 1.0, None),
    };
    let tool = ["swift", "build", "--product", "Tool"];

    let one = predict(
        &history_for(std::slice::from_ref(&app), &tool, PLACE),
        7.5,
        8.0,
    );
    assert_eq!(one.level, Level::None);

    let two = predict(
        &history_for(&[app.clone(), kit.clone()], &tool, PLACE),
        7.5,
        8.0,
    );
    assert_eq!(two.level, Level::Head);
    assert_close(two.cores, 3.0);
    assert_eq!(two.basis(), "2 runs of the same program and flags");

    let three = predict(
        &history_for(
            &[app, kit, verbose],
            &["swift", "build", "--verbose"],
            PLACE,
        ),
        7.5,
        8.0,
    );
    assert_eq!(three.level, Level::Prog);
    assert_eq!(three.runs, 3);
    assert_close(three.cores, 2.0);
    assert_eq!(three.basis(), "3 runs of the same program in this place");
}

#[test]
fn one_run_of_the_same_command_is_enough_for_the_full_level() {
    let past = [Past {
        argv: &["swift", "test", "--parallel"],
        place: PLACE,
        run: run(10, 2.0, Some(500)),
    }];
    let p = predict(
        &history_for(&past, &["swift", "test", "--parallel"], PLACE),
        7.5,
        8.0,
    );
    assert_eq!(
        p,
        Prediction {
            cores: 2.0,
            rss_kib: Some(500),
            rss_p90_kib: Some(500),
            duration_s: 10.0,
            duration_p90_s: Some(10.0),
            level: Level::Full,
            runs: 1,
        }
    );
    assert_eq!(p.basis(), "1 run of the same command");
}

#[test]
fn a_finer_level_with_one_run_beats_a_coarser_level_with_more() {
    let past = [
        Past {
            argv: &["swift", "test", "--parallel", "Foo"],
            place: PLACE,
            run: run(10, 4.0, None),
        },
        Past {
            argv: &["swift", "test", "--parallel"],
            place: PLACE,
            run: run(10, 2.0, None),
        },
    ];
    let p = predict(
        &history_for(&past, &["swift", "test", "--parallel"], PLACE),
        7.5,
        8.0,
    );
    assert_eq!(p.level, Level::Full);
    assert_eq!(p.runs, 1);
    assert_close(p.cores, 2.0);
}

#[test]
fn head_needs_two_runs_of_the_same_program_and_flags() {
    let mut past = vec![Past {
        argv: &["swift", "test", "--parallel", "Foo"],
        place: PLACE,
        run: run(10, 2.0, None),
    }];
    let query = ["swift", "test", "--parallel"];
    assert_eq!(
        predict(&history_for(&past, &query, PLACE), 7.5, 8.0),
        Prediction {
            cores: 7.5,
            rss_kib: None,
            rss_p90_kib: None,
            duration_s: 10.0,
            duration_p90_s: None,
            level: Level::None,
            runs: 0,
        }
    );
    past.push(Past {
        argv: &["swift", "test", "--parallel", "Bar"],
        place: PLACE,
        run: run(20, 6.0, None),
    });
    let p = predict(&history_for(&past, &query, PLACE), 7.5, 8.0);
    assert_eq!(p.level, Level::Head);
    assert_eq!(p.runs, 2);
    assert_close(p.cores, 4.0);
    assert_close(p.duration_s, 15.0);
    assert_eq!(p.duration_p90_s, Some(20.0));
    assert_eq!(p.basis(), "2 runs of the same program and flags");
}

#[test]
fn prog_needs_three_runs_of_the_program_in_the_same_place() {
    let mut past = vec![
        Past {
            argv: &["swift", "test", "--filter", "A"],
            place: PLACE,
            run: run(10, 1.0, None),
        },
        Past {
            argv: &["swift", "test", "--filter", "B"],
            place: PLACE,
            run: run(10, 1.0, None),
        },
    ];
    let query = ["swift", "test", "--parallel"];
    assert_eq!(
        predict(&history_for(&past, &query, PLACE), 7.5, 8.0).level,
        Level::None
    );
    past.push(Past {
        argv: &["swift", "test", "-v"],
        place: PLACE,
        run: run(10, 1.0, None),
    });
    let p = predict(&history_for(&past, &query, PLACE), 7.5, 8.0);
    assert_eq!(p.level, Level::Prog);
    assert_eq!(p.runs, 3);
    assert_eq!(p.basis(), "3 runs of the same program in this place");
}

#[test]
fn tool_needs_three_runs_of_the_program_in_any_place() {
    let mut past: Vec<Past> = (0..2)
        .map(|_| Past {
            argv: &["swift", "test", "--filter", "A"],
            place: OTHER,
            run: run(10, 1.0, None),
        })
        .collect();
    let query = ["swift", "test", "--parallel"];
    assert_eq!(
        predict(&history_for(&past, &query, PLACE), 7.5, 8.0).level,
        Level::None
    );
    past.push(Past {
        argv: &["swift", "test", "--filter", "B"],
        place: OTHER,
        run: run(10, 1.0, None),
    });
    let p = predict(&history_for(&past, &query, PLACE), 7.5, 8.0);
    assert_eq!(p.level, Level::Tool);
    assert_eq!(p.runs, 3);
    assert_eq!(p.basis(), "3 runs of the same program");
}

#[test]
fn exclusive_cold_start_predicts_one_core_capped_by_the_weight() {
    assert_eq!(
        predict(&History::default(), 8.0, 8.0),
        Prediction {
            cores: 1.0,
            rss_kib: None,
            rss_p90_kib: None,
            duration_s: 30.0,
            duration_p90_s: None,
            level: Level::Exclusive,
            runs: 0,
        }
    );
    assert_eq!(
        predict(&History::default(), 8.0, 8.0).basis(),
        "typical exclusive use; no history"
    );
}

#[test]
fn typical_exclusive_use_is_the_median_of_the_exclusive_runs() {
    let h = History {
        exclusive_runs: vec![run(10, 0.5, None), run(10, 1.2, None), run(10, 0.8, None)],
        ..History::default()
    };
    let p = predict(&h, 8.0, 8.0);
    assert_eq!(p.level, Level::Exclusive);
    assert_close(p.cores, 0.8);
}

#[test]
fn typical_exclusive_use_is_capped_by_the_weight_when_the_runs_used_more() {
    let mut runs = vec![run(10, 10.0, None); 20];
    runs.extend(vec![run(10, 0.2, None); 3]);
    let h = History {
        exclusive_runs: runs,
        ..History::default()
    };
    assert_close(predict(&h, 8.0, 8.0).cores, 8.0);
}

#[test]
fn fewer_than_three_exclusive_runs_predict_one_core() {
    let h = History {
        exclusive_runs: vec![run(10, 5.0, None), run(10, 5.0, None)],
        ..History::default()
    };
    assert_close(predict(&h, 8.0, 8.0).cores, 1.0);
}

#[test]
fn exclusive_needs_at_least_two_cpus_and_a_weight_of_all_of_them() {
    assert_eq!(
        predict(&History::default(), 2.0, 2.0).level,
        Level::Exclusive
    );
    assert_eq!(predict(&History::default(), 8.0, 1.0).level, Level::None);
    assert_eq!(predict(&History::default(), 3.5, 4.0).level, Level::None);
}
