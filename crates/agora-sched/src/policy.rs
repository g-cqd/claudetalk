//! Team and lane policies with layered defaults.

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TeamPolicy {
    pub stale_after_s: Option<f64>,
    pub escalate_after_s: Option<f64>,
    pub reclaim_after_s: Option<f64>,
    pub reclaim: Option<bool>,
    pub require_review: Option<bool>,
    pub wip_limit: Option<u32>,
    /// hex pubkeys, notified in order at escalation.
    pub escalation_chain: Option<Vec<String>>,
    pub sprint_rollover: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EffectivePolicy {
    pub stale_after_s: f64,
    pub escalate_after_s: f64,
    pub reclaim_after_s: f64,
    pub reclaim: bool,
    pub require_review: bool,
    pub wip_limit: Option<u32>,
    pub escalation_chain: Vec<String>,
    pub sprint_rollover: bool,
}

/// defaults: stale 1800, escalate 900, reclaim 3600, reclaim true, require_review = given default, wip None, chain [],
/// rollover true. `layers` most general first: ("*","*"), (team,"*"), ("*",lane), (team,lane); later Some() wins.
pub fn effective(layers: &[&TeamPolicy], require_review_default: bool) -> EffectivePolicy {
    let mut out = EffectivePolicy {
        stale_after_s: 1800.0,
        escalate_after_s: 900.0,
        reclaim_after_s: 3600.0,
        reclaim: true,
        require_review: require_review_default,
        wip_limit: None,
        escalation_chain: Vec::new(),
        sprint_rollover: true,
    };
    for layer in layers {
        if let Some(v) = layer.stale_after_s {
            out.stale_after_s = v;
        }
        if let Some(v) = layer.escalate_after_s {
            out.escalate_after_s = v;
        }
        if let Some(v) = layer.reclaim_after_s {
            out.reclaim_after_s = v;
        }
        if let Some(v) = layer.reclaim {
            out.reclaim = v;
        }
        if let Some(v) = layer.require_review {
            out.require_review = v;
        }
        if let Some(v) = layer.wip_limit {
            out.wip_limit = Some(v);
        }
        if let Some(chain) = &layer.escalation_chain {
            out.escalation_chain = chain.clone();
        }
        if let Some(v) = layer.sprint_rollover {
            out.sprint_rollover = v;
        }
    }
    out
}

/// durations finite > 0, wip >= 1, chain items 64-hex lowercase, <= 16
pub fn validate(p: &TeamPolicy) -> Result<(), String> {
    let durations = [
        ("stale_after_s", p.stale_after_s),
        ("escalate_after_s", p.escalate_after_s),
        ("reclaim_after_s", p.reclaim_after_s),
    ];
    for (name, value) in durations {
        if value.is_some_and(|v| !v.is_finite() || v <= 0.0) {
            return Err(format!("{name} must be a finite number greater than zero"));
        }
    }
    if p.wip_limit.is_some_and(|w| w < 1) {
        return Err("wip_limit must be at least 1".to_string());
    }
    if let Some(chain) = &p.escalation_chain {
        if chain.len() > 16 {
            return Err("escalation_chain must have at most 16 entries".to_string());
        }
        if !chain.iter().all(|k| is_pubkey_hex(k)) {
            return Err(
                "escalation_chain entries must be 64-character lowercase hex public keys"
                    .to_string(),
            );
        }
        let mut seen = std::collections::HashSet::new();
        if !chain.iter().all(|k| seen.insert(k.as_str())) {
            return Err("escalation_chain must not contain duplicate entries".to_string());
        }
    }
    Ok(())
}

/// Exactly 64 bytes, each `0-9` or `a-f`.
fn is_pubkey_hex(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64-character lowercase hex key derived from `n`.
    fn key(n: u64) -> String {
        format!("{n:064x}")
    }

    fn with(f: impl FnOnce(&mut TeamPolicy)) -> TeamPolicy {
        let mut p = TeamPolicy::default();
        f(&mut p);
        p
    }

    #[test]
    fn defaults_with_no_layers() {
        let p = effective(&[], true);
        assert_eq!(
            p,
            EffectivePolicy {
                stale_after_s: 1800.0,
                escalate_after_s: 900.0,
                reclaim_after_s: 3600.0,
                reclaim: true,
                require_review: true,
                wip_limit: None,
                escalation_chain: Vec::new(),
                sprint_rollover: true,
            }
        );
    }

    #[test]
    fn require_review_default_passes_through_when_no_layer_sets_it() {
        assert!(effective(&[], true).require_review);
        assert!(!effective(&[], false).require_review);
        let unset = TeamPolicy::default();
        assert!(effective(&[&unset], true).require_review);
        assert!(!effective(&[&unset], false).require_review);
    }

    #[test]
    fn require_review_layer_overrides_the_default() {
        let on = with(|p| p.require_review = Some(true));
        let off = with(|p| p.require_review = Some(false));
        assert!(effective(&[&on], false).require_review);
        assert!(!effective(&[&off], true).require_review);
    }

    #[test]
    fn most_specific_layer_wins_for_each_field() {
        let global = with(|p| {
            p.stale_after_s = Some(100.0);
            p.escalate_after_s = Some(100.0);
            p.reclaim_after_s = Some(100.0);
        });
        let team = with(|p| {
            p.stale_after_s = Some(200.0);
            p.escalate_after_s = Some(200.0);
        });
        let lane = with(|p| p.stale_after_s = Some(300.0));
        let team_lane = with(|p| p.stale_after_s = Some(400.0));
        let e = effective(&[&global, &team, &lane, &team_lane], true);
        assert_eq!(e.stale_after_s, 400.0, "team+lane beats lane");
        assert_eq!(e.escalate_after_s, 200.0, "team beats global");
        assert_eq!(
            e.reclaim_after_s, 100.0,
            "global applies when nothing overrides"
        );
    }

    #[test]
    fn lane_beats_team_and_team_beats_global_without_team_lane() {
        let global = with(|p| p.stale_after_s = Some(100.0));
        let team = with(|p| p.stale_after_s = Some(200.0));
        let lane = with(|p| p.stale_after_s = Some(300.0));
        assert_eq!(effective(&[&global, &team], true).stale_after_s, 200.0);
        assert_eq!(
            effective(&[&global, &team, &lane], true).stale_after_s,
            300.0
        );
    }

    #[test]
    fn layers_apply_in_the_order_given() {
        let a = with(|p| p.stale_after_s = Some(1.0));
        let b = with(|p| p.stale_after_s = Some(2.0));
        assert_eq!(effective(&[&a, &b], true).stale_after_s, 2.0);
        assert_eq!(effective(&[&b, &a], true).stale_after_s, 1.0);
    }

    #[test]
    fn partial_layer_keeps_every_other_default() {
        let p = with(|p| p.wip_limit = Some(3));
        let e = effective(&[&p], false);
        assert_eq!(e.wip_limit, Some(3));
        assert_eq!(e.stale_after_s, 1800.0);
        assert_eq!(e.escalate_after_s, 900.0);
        assert_eq!(e.reclaim_after_s, 3600.0);
        assert!(e.reclaim);
        assert!(!e.require_review);
        assert!(e.escalation_chain.is_empty());
        assert!(e.sprint_rollover);
    }

    #[test]
    fn none_fields_do_not_reset_earlier_layers() {
        let global = with(|p| {
            p.wip_limit = Some(3);
            p.reclaim = Some(false);
        });
        let team = TeamPolicy::default();
        let e = effective(&[&global, &team], true);
        assert_eq!(e.wip_limit, Some(3));
        assert!(!e.reclaim);
    }

    #[test]
    fn escalation_chain_is_replaced_not_appended() {
        let global = with(|p| p.escalation_chain = Some(vec![key(1), key(2)]));
        let team = with(|p| p.escalation_chain = Some(vec![key(3)]));
        assert_eq!(
            effective(&[&global, &team], true).escalation_chain,
            vec![key(3)]
        );
        let cleared = with(|p| p.escalation_chain = Some(Vec::new()));
        assert!(
            effective(&[&global, &cleared], true)
                .escalation_chain
                .is_empty()
        );
    }

    #[test]
    fn reclaim_and_sprint_rollover_can_be_turned_off() {
        let p = with(|p| {
            p.reclaim = Some(false);
            p.sprint_rollover = Some(false);
        });
        let e = effective(&[&p], true);
        assert!(!e.reclaim);
        assert!(!e.sprint_rollover);
    }

    #[test]
    fn validate_accepts_default_and_boundary_values() {
        assert!(validate(&TeamPolicy::default()).is_ok());
        let p = with(|p| {
            p.stale_after_s = Some(f64::MIN_POSITIVE);
            p.escalate_after_s = Some(1e9);
            p.reclaim_after_s = Some(1.0);
            p.wip_limit = Some(1);
            p.escalation_chain = Some(Vec::new());
        });
        assert!(validate(&p).is_ok());
    }

    #[test]
    fn validate_rejects_non_positive_or_non_finite_durations_by_name() {
        let bad = [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY];
        for v in bad {
            let cases = [
                (with(|p| p.stale_after_s = Some(v)), "stale_after_s"),
                (with(|p| p.escalate_after_s = Some(v)), "escalate_after_s"),
                (with(|p| p.reclaim_after_s = Some(v)), "reclaim_after_s"),
            ];
            for (p, name) in cases {
                assert_eq!(
                    validate(&p),
                    Err(format!("{name} must be a finite number greater than zero")),
                    "value {v}"
                );
            }
        }
    }

    #[test]
    fn validate_rejects_wip_limit_zero() {
        let p = with(|p| p.wip_limit = Some(0));
        assert_eq!(
            validate(&p),
            Err("wip_limit must be at least 1".to_string())
        );
    }

    #[test]
    fn validate_accepts_sixteen_distinct_keys_and_rejects_seventeen() {
        let sixteen = with(|p| p.escalation_chain = Some((0..16).map(key).collect()));
        assert!(validate(&sixteen).is_ok());
        let seventeen = with(|p| p.escalation_chain = Some((0..17).map(key).collect()));
        assert_eq!(
            validate(&seventeen),
            Err("escalation_chain must have at most 16 entries".to_string())
        );
    }

    #[test]
    fn validate_rejects_malformed_chain_entries() {
        let entry_error = "escalation_chain entries must be 64-character lowercase hex public keys";
        let bad = [
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            "g".repeat(64),
            format!("0x{}", "a".repeat(62)),
            String::new(),
        ];
        for entry in bad {
            let p = with(|p| p.escalation_chain = Some(vec![key(7), entry.clone()]));
            assert_eq!(
                validate(&p),
                Err(entry_error.to_string()),
                "entry {entry:?}"
            );
        }
    }

    #[test]
    fn validate_rejects_duplicate_chain_entries() {
        let p = with(|p| p.escalation_chain = Some(vec![key(1), key(2), key(1)]));
        assert_eq!(
            validate(&p),
            Err("escalation_chain must not contain duplicate entries".to_string())
        );
    }

    #[test]
    fn serde_round_trip_preserves_every_field() {
        let p = TeamPolicy {
            stale_after_s: Some(120.5),
            escalate_after_s: Some(60.0),
            reclaim_after_s: Some(600.0),
            reclaim: Some(false),
            require_review: Some(true),
            wip_limit: Some(4),
            escalation_chain: Some(vec![key(9)]),
            sprint_rollover: Some(false),
        };
        let text = serde_json::to_string(&p).expect("serialize");
        let back: TeamPolicy = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, p);
        let e = effective(&[&p], true);
        let text = serde_json::to_string(&e).expect("serialize effective");
        let back: EffectivePolicy = serde_json::from_str(&text).expect("deserialize effective");
        assert_eq!(back, e);
    }

    #[test]
    fn serde_missing_keys_default_to_none() {
        let p: TeamPolicy = serde_json::from_str("{}").expect("empty object");
        assert_eq!(p, TeamPolicy::default());
    }

    #[test]
    fn serde_rejects_unknown_keys() {
        let err =
            serde_json::from_str::<TeamPolicy>(r#"{"stale_after_s": 5.0, "stale_after": 5.0}"#)
                .expect_err("unknown key must be refused");
        assert!(err.to_string().contains("unknown field"), "{err}");
    }
}
