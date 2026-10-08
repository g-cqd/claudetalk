//! Resource prediction from run history (ref-admission section 2).

/// The runs of one signature level that a prediction reads: the newest, at most this many.
const HISTORY: usize = 5;
/// The recent run durations that stand in for a job without history: the newest, at most this many.
const RECENT: usize = 200;
/// The exclusive runs whose median use stands in for an exclusive job without history: the newest, at most this many.
const TYPICAL_RUNS: usize = 20;
/// Fewer exclusive runs than this: `TYPICAL_CORES` stands in.
const TYPICAL_MIN: usize = 3;
const TYPICAL_CORES: f64 = 1.0;
/// The run time of a job without history when no job has run yet, in seconds.
const DEFAULT_DURATION: f64 = 30.0;
const MIN_CORES: f64 = 0.1;
/// A run lasts at least this many seconds, so that a run too short to sample does not divide by zero.
const MIN_DURATION: f64 = 0.05;
const P90: f64 = 0.9;
/// The signature levels, finest first. `NEEDED[i]` is the number of runs `LEVELS[i]` needs.
const LEVELS: [Level; 4] = [Level::Full, Level::Head, Level::Prog, Level::Tool];

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Run {
    pub started_ms: i64,
    pub ended_ms: i64,
    pub cpu_seconds: f64,
    pub rss_peak_kib: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Full,
    Head,
    Prog,
    Tool,
    Exclusive,
    None,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Prediction {
    pub cores: f64,
    pub rss_kib: Option<u64>,
    pub rss_p90_kib: Option<u64>,
    pub duration_s: f64,
    pub duration_p90_s: Option<f64>,
    pub level: Level,
    pub runs: u32,
}

impl Prediction {
    /// "no history: the declared weight" | "typical exclusive use; no history" | "N run(s) of <source>".
    pub fn basis(&self) -> String {
        let source = match self.level {
            Level::None => return "no history: the declared weight".to_string(),
            Level::Exclusive => return "typical exclusive use; no history".to_string(),
            Level::Full => "the same command",
            Level::Head => "the same program and flags",
            Level::Prog => "the same program in this place",
            Level::Tool => "the same program",
        };
        let plural = if self.runs == 1 { "" } else { "s" };
        format!("{} run{plural} of {source}", self.runs)
    }
}

/// Pre-fetched history. by_level = [full, head, prog, tool], each newest first, at most 5 RAN runs (status not null and
/// not 126/127, started/ended/cpu_seconds present); recent_durations_s = up to 200 most recent RAN durations (any
/// command, seconds, no floor); exclusive_runs = up to 20 most recent RAN runs whose weight >= cpus.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct History {
    pub by_level: [Vec<Run>; 4],
    pub recent_durations_s: Vec<f64>,
    pub exclusive_runs: Vec<Run>,
}

pub const NEEDED: [usize; 4] = [1, 2, 3, 3];

/// The median of `xs`: the middle value of an odd count, the mean of the two middle values of an even count. None when
/// `xs` is empty.
pub fn median(xs: &[f64]) -> Option<f64> {
    let mut sorted = xs.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted.get(mid).copied()
    } else {
        Some((*sorted.get(mid.checked_sub(1)?)? + *sorted.get(mid)?) / 2.0)
    }
}

/// nearest rank: sorted[max(0, ceil(q*n)-1)].
pub fn percentile(xs: &[f64], q: f64) -> Option<f64> {
    let mut sorted = xs.to_vec();
    sorted.sort_by(f64::total_cmp);
    nearest_rank(&sorted, q)
}

/// The nearest-rank element of an ascending slice, `sorted[max(0, ceil(q*n)-1)]`, with the index clamped to the last
/// element. None when the slice is empty.
fn nearest_rank<T: Copy>(sorted: &[T], q: f64) -> Option<T> {
    let last = sorted.len().checked_sub(1)?;
    // A float-to-integer `as` saturates and maps NaN to 0, so a degenerate `q` lands on the first element.
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.saturating_sub(1).min(last)).copied()
}

/// Exclusive use is for a machine with at least two CPUs, and a job that declares at least all of them.
pub fn exclusive_at(weight: f64, cpus: f64) -> bool {
    cpus >= 2.0 && weight >= cpus
}

/// The cores an exclusive job without history is predicted to use: the median of the average cores of the newest 20
/// exclusive runs, floored at a tenth of a core, or 1 core while fewer than 3 runs exist. The caller supplies only
/// runs whose weight is at least the CPU count.
pub fn typical_exclusive(runs: &[Run]) -> f64 {
    let runs = newest(runs, TYPICAL_RUNS);
    if runs.len() < TYPICAL_MIN {
        return TYPICAL_CORES;
    }
    let rates: Vec<f64> = runs
        .iter()
        .map(|run| run.cpu_seconds / run_seconds(run))
        .collect();
    median(&rates).map_or(TYPICAL_CORES, |cores| cores.max(MIN_CORES))
}

/// The prediction for a job of declared `weight` on a machine with `cpus` CPUs: the first level, finest first, with
/// enough runs; else, without history, the declared weight, the median run time of recent jobs, and the typical
/// exclusive use when the job is exclusive.
pub fn predict(h: &History, weight: f64, cpus: f64) -> Prediction {
    for ((level, needed), history) in LEVELS.into_iter().zip(NEEDED).zip(&h.by_level) {
        let runs = newest(history, HISTORY);
        if runs.len() < needed {
            continue;
        }
        if let Some(prediction) = from_level(level, runs, weight) {
            return prediction;
        }
    }
    let duration_s = median(newest(&h.recent_durations_s, RECENT)).unwrap_or(DEFAULT_DURATION);
    if exclusive_at(weight, cpus) {
        return Prediction {
            cores: weight.min(typical_exclusive(&h.exclusive_runs)),
            rss_kib: None,
            rss_p90_kib: None,
            duration_s,
            duration_p90_s: None,
            level: Level::Exclusive,
            runs: 0,
        };
    }
    Prediction {
        cores: weight,
        rss_kib: None,
        rss_p90_kib: None,
        duration_s,
        duration_p90_s: None,
        level: Level::None,
        runs: 0,
    }
}

/// The prediction a level gives from its runs. None when `runs` is empty, which no caller passes.
fn from_level(level: Level, runs: &[Run], weight: f64) -> Option<Prediction> {
    let durations: Vec<f64> = runs.iter().map(run_seconds).collect();
    let rates: Vec<f64> = runs
        .iter()
        .zip(&durations)
        .map(|(run, secs)| run.cpu_seconds / secs)
        .collect();
    let mut peaks: Vec<u64> = runs.iter().filter_map(|run| run.rss_peak_kib).collect();
    peaks.sort_unstable();
    Some(Prediction {
        cores: weight.min(median(&rates)?.max(MIN_CORES)),
        rss_kib: peaks.last().copied(),
        rss_p90_kib: nearest_rank(&peaks, P90),
        duration_s: median(&durations)?,
        duration_p90_s: percentile(&durations, P90),
        level,
        runs: u32::try_from(runs.len()).unwrap_or(u32::MAX),
    })
}

/// A run's wall time in seconds, floored at `MIN_DURATION`. The difference saturates, so a corrupt pair of times
/// cannot overflow.
fn run_seconds(run: &Run) -> f64 {
    (run.ended_ms.saturating_sub(run.started_ms) as f64 / 1000.0).max(MIN_DURATION)
}

/// The first `cap` items of a newest-first slice; the whole slice when it is shorter.
fn newest<T>(xs: &[T], cap: usize) -> &[T] {
    xs.get(..cap).unwrap_or(xs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run of `secs` whole seconds that used `cpu` CPU-seconds, with the given peak.
    fn run(secs: i64, cpu: f64, peak: Option<u64>) -> Run {
        Run {
            started_ms: 1_000,
            ended_ms: 1_000 + secs * 1_000,
            cpu_seconds: cpu,
            rss_peak_kib: peak,
        }
    }

    /// History with `runs` at signature level `level` (0 = full .. 3 = tool).
    fn at(level: usize, runs: Vec<Run>) -> History {
        let mut h = History::default();
        h.by_level[level] = runs;
        h
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn median_takes_the_middle_of_an_odd_count() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[5.0]), Some(5.0));
    }

    #[test]
    fn median_averages_the_two_middle_values_of_an_even_count() {
        assert_eq!(median(&[1.0, 2.0]), Some(1.5));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }

    #[test]
    fn median_of_nothing_is_none() {
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn percentile_is_nearest_rank_for_one_to_ten_values() {
        // ceil(0.9 n) for n = 1..10 is 1,2,3,4,5,6,7,8,9,9: up to five values the p90 is the maximum.
        let expected = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 9.0];
        for (n, want) in (1..=10).zip(expected) {
            let xs: Vec<f64> = (1..=n).map(f64::from).collect();
            assert_eq!(percentile(&xs, 0.9), Some(want), "n = {n}");
        }
    }

    #[test]
    fn percentile_of_nothing_is_none() {
        assert_eq!(percentile(&[], 0.9), None);
    }

    #[test]
    fn exclusive_at_needs_two_cpus_and_a_weight_of_all_of_them() {
        assert!(!exclusive_at(1.0, 1.0));
        assert!(!exclusive_at(1.0, 0.5));
        assert!(exclusive_at(2.0, 2.0));
        assert!(!exclusive_at(3.5, 4.0));
        assert!(exclusive_at(8.0, 8.0));
    }

    #[test]
    fn typical_exclusive_is_one_core_below_three_runs() {
        assert_eq!(typical_exclusive(&[]), 1.0);
        assert_eq!(
            typical_exclusive(&[run(10, 5.0, None), run(10, 0.5, None)]),
            1.0
        );
    }

    #[test]
    fn typical_exclusive_is_the_median_use_floored_at_a_tenth_of_a_core() {
        let runs = [run(1, 0.5, None), run(1, 1.2, None), run(1, 0.8, None)];
        assert_close(typical_exclusive(&runs), 0.8);
        let idle = [run(1, 0.0, None), run(1, 0.0, None), run(1, 0.0, None)];
        assert_close(typical_exclusive(&idle), 0.1);
    }

    #[test]
    fn typical_exclusive_reads_only_the_twenty_newest_runs() {
        let mut runs = vec![run(10, 20.0, None); 20];
        runs.extend(vec![run(10, 500.0, None); 40]);
        assert_close(typical_exclusive(&runs), 2.0);
    }

    #[test]
    fn exclusive_cold_start_predicts_one_core_capped_by_the_weight() {
        let p = predict(&History::default(), 8.0, 8.0);
        assert_eq!(p.level, Level::Exclusive);
        assert_close(p.cores, 1.0);
        assert_eq!(
            (p.rss_kib, p.rss_p90_kib, p.duration_p90_s, p.runs),
            (None, None, None, 0)
        );
        assert_eq!(p.basis(), "typical exclusive use; no history");
    }

    #[test]
    fn exclusive_prediction_is_the_typical_use_of_history() {
        let h = History {
            exclusive_runs: vec![run(10, 5.0, None), run(10, 12.0, None), run(10, 8.0, None)],
            ..History::default()
        };
        assert_close(predict(&h, 8.0, 8.0).cores, 0.8);
        let busy = History {
            exclusive_runs: vec![run(10, 100.0, None); 20],
            ..History::default()
        };
        assert_close(predict(&busy, 8.0, 8.0).cores, 8.0);
    }

    #[test]
    fn no_history_declares_the_weight_with_a_thirty_second_run() {
        let p = predict(&History::default(), 3.0, 8.0);
        assert_eq!(p.level, Level::None);
        assert_close(p.cores, 3.0);
        assert_eq!(p.duration_s, 30.0);
        assert_eq!(
            (p.rss_kib, p.rss_p90_kib, p.duration_p90_s, p.runs),
            (None, None, None, 0)
        );
        assert_eq!(p.basis(), "no history: the declared weight");
    }

    #[test]
    fn no_history_takes_the_median_run_time_of_recent_jobs() {
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
    fn recent_durations_are_not_floored_and_read_only_the_200_newest() {
        let h = History {
            recent_durations_s: vec![0.01],
            ..History::default()
        };
        assert_eq!(predict(&h, 1.0, 1.0).duration_s, 0.01);
        let mut many = vec![1.0; 200];
        many.extend(vec![1000.0; 300]);
        let h = History {
            recent_durations_s: many,
            ..History::default()
        };
        assert_eq!(predict(&h, 1.0, 1.0).duration_s, 1.0);
    }

    #[test]
    fn full_level_reads_the_five_newest_runs_and_ignores_older_ones() {
        let runs = vec![
            run(60, 60.0, Some(400_000)),
            run(80, 120.0, Some(900_000)),
            run(50, 40.0, Some(300_000)),
            run(70, 210.0, Some(500_000)),
            run(65, 78.0, Some(450_000)),
            run(500, 3500.0, Some(9_000_000)),
        ];
        let p = predict(&at(0, runs), 7.5, 8.0);
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
    fn prediction_is_capped_by_the_weight_and_floored_at_a_tenth_of_a_core() {
        let fast = at(0, vec![run(10, 60.0, None)]);
        assert_close(predict(&fast, 2.0, 8.0).cores, 2.0);
        let idle = at(0, vec![run(10, 0.0, None)]);
        assert_close(predict(&idle, 8.0, 8.0).cores, 0.1);
        let tiny = at(0, vec![run(10, 10.0, None)]);
        assert_close(predict(&tiny, 0.05, 8.0).cores, 0.05);
        assert_close(predict(&History::default(), 0.05, 8.0).cores, 0.05);
    }

    #[test]
    fn a_zero_length_run_is_floored_to_a_twentieth_of_a_second() {
        let h = at(
            0,
            vec![Run {
                started_ms: 5_000,
                ended_ms: 5_000,
                cpu_seconds: 0.05,
                rss_peak_kib: None,
            }],
        );
        let p = predict(&h, 8.0, 8.0);
        assert_close(p.duration_s, 0.05);
        assert_close(p.cores, 1.0);
    }

    #[test]
    fn head_level_needs_two_runs_and_takes_the_median_of_an_even_count() {
        let p = predict(
            &at(1, vec![run(100, 400.0, None), run(50, 100.0, None)]),
            7.5,
            8.0,
        );
        assert_eq!(p.level, Level::Head);
        assert_close(p.cores, 3.0);
        assert_eq!(p.basis(), "2 runs of the same program and flags");
        let even = predict(
            &at(1, vec![run(10, 10.0, None), run(10, 20.0, None)]),
            8.0,
            8.0,
        );
        assert_close(even.cores, 1.5);
    }

    #[test]
    fn a_finer_level_with_too_few_runs_falls_through_to_a_coarser_one() {
        let mut h = History::default();
        h.by_level[1] = vec![run(10, 10.0, None)];
        h.by_level[2] = vec![run(10, 20.0, None); 3];
        let p = predict(&h, 8.0, 8.0);
        assert_eq!(p.level, Level::Prog);
        assert_eq!(p.basis(), "3 runs of the same program in this place");
    }

    #[test]
    fn tool_level_needs_three_runs_and_reports_the_same_program() {
        let h = at(3, vec![run(10, 10.0, None); 3]);
        let p = predict(&h, 8.0, 8.0);
        assert_eq!(p.level, Level::Tool);
        assert_eq!(p.basis(), "3 runs of the same program");
        let short = at(3, vec![run(10, 10.0, None); 2]);
        assert_eq!(predict(&short, 3.0, 8.0).level, Level::None);
    }

    #[test]
    fn peaks_are_summarised_without_the_missing_ones() {
        let all_missing = predict(&at(0, vec![run(10, 10.0, None)]), 8.0, 8.0);
        assert_eq!((all_missing.rss_kib, all_missing.rss_p90_kib), (None, None));
        let mixed = predict(
            &at(
                1,
                vec![
                    run(10, 10.0, None),
                    run(10, 10.0, Some(300)),
                    run(10, 10.0, Some(100)),
                ],
            ),
            8.0,
            8.0,
        );
        assert_eq!((mixed.rss_kib, mixed.rss_p90_kib), (Some(300), Some(300)));
    }

    #[test]
    fn basis_strings_for_every_level_and_run_count() {
        let p = |level, runs| Prediction {
            cores: 1.0,
            rss_kib: None,
            rss_p90_kib: None,
            duration_s: 1.0,
            duration_p90_s: None,
            level,
            runs,
        };
        assert_eq!(p(Level::Full, 1).basis(), "1 run of the same command");
        assert_eq!(p(Level::Full, 2).basis(), "2 runs of the same command");
        assert_eq!(
            p(Level::Head, 2).basis(),
            "2 runs of the same program and flags"
        );
        assert_eq!(
            p(Level::Prog, 1).basis(),
            "1 run of the same program in this place"
        );
        assert_eq!(p(Level::Tool, 4).basis(), "4 runs of the same program");
        assert_eq!(p(Level::None, 0).basis(), "no history: the declared weight");
        assert_eq!(
            p(Level::Exclusive, 0).basis(),
            "typical exclusive use; no history"
        );
    }
}
