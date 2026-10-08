//! Queue order, ref-admission section 5 (table B), through `admit::waiting_order_cmp`.

use super::*;

// ---- B. Queue order (O1-O9) ----

#[test]
fn o1_the_job_with_the_shorter_predicted_finish_goes_first() {
    let long = queued("long", 0.0, Some(600.0));
    let short = queued("short", 100.0, Some(30.0));
    assert_eq!(order(&[long, short], 1.0), ["short", "long"]);
}

#[test]
fn o2_a_late_short_job_loses_to_a_long_one_at_610_against_600() {
    let long = queued("long", 0.0, Some(600.0));
    let late = queued("late", 580.0, Some(30.0));
    assert_eq!(order(&[long, late], 1.0), ["long", "late"]);
}

#[test]
fn o3_equal_keys_break_on_the_earlier_request_whatever_the_input_order() {
    let long = queued("long", 0.0, Some(600.0));
    let short = queued("short", 570.0, Some(30.0));
    assert_eq!(order(&[short, long], 1.0), ["long", "short"]);
}

#[test]
fn o4_a_validity_job_goes_before_a_shorter_job() {
    let short = WaitingJob {
        kind: "other".to_owned(),
        ..queued("short", 50.0, Some(1.0))
    };
    let fix = WaitingJob {
        kind: "validity".to_owned(),
        ..queued("fix", 60.0, Some(900.0))
    };
    assert_eq!(order(&[short, fix], 1.0), ["fix", "short"]);
}

#[test]
fn o5_a_job_without_a_prediction_counts_as_thirty_seconds() {
    let unknown = queued("unknown", 0.0, None);
    let known = queued("known", 0.0, Some(60.0));
    assert_eq!(order(&[known, unknown.clone()], 1.0), ["unknown", "known"]);
    let quick = queued("known", 0.0, Some(10.0));
    assert_eq!(order(&[unknown, quick], 1.0), ["known", "unknown"]);
}

#[test]
fn o6_with_no_aging_the_shorter_job_always_goes_first() {
    let long = queued("long", 0.0, Some(600.0));
    let late_short = queued("short", 1_000_000.0, Some(30.0));
    assert_eq!(
        order(&[long.clone(), late_short.clone()], 0.0),
        ["short", "long"]
    );
    assert_eq!(order(&[long, late_short], 1.0), ["long", "short"]);
}

#[test]
fn o7_aging_puts_the_short_job_ahead_only_while_its_key_stays_below_600() {
    let long = queued("long", 0.0, Some(600.0));
    let at = |requested: f64| queued("short", requested, Some(30.0));
    // Keys: long 600; short 30 + 0.5 * requested. Ahead iff requested < 1140; at 1140 the tie goes to the earlier request.
    assert_eq!(order(&[long.clone(), at(1139.0)], 0.5), ["short", "long"]);
    assert_eq!(order(&[long.clone(), at(1140.0)], 0.5), ["long", "short"]);
    assert_eq!(order(&[long, at(1141.0)], 0.5), ["long", "short"]);
}

#[test]
fn o8_the_validity_kind_is_matched_case_insensitively() {
    let short = queued("short", 50.0, Some(1.0));
    let fix = WaitingJob {
        kind: "Validity".to_owned(),
        ..queued("fix", 60.0, Some(900.0))
    };
    assert_eq!(order(&[short, fix], 1.0), ["fix", "short"]);
}

#[test]
fn o9_identical_jobs_keep_the_lower_sequence_first() {
    let a = WaitingJob {
        seq: 2,
        ..queued("a", 0.0, Some(60.0))
    };
    let b = WaitingJob {
        seq: 1,
        ..queued("b", 0.0, Some(60.0))
    };
    assert_eq!(order(&[a, b], 1.0), ["b", "a"]);
}
