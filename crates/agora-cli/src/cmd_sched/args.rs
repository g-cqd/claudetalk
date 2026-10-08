//! Helpers of `cmd_sched`: the clap-flag -> API-args mapping and its client-side checks. Each refuses, with a usage
//! error (exit 2), input that the daemon would refuse anyway; the mapping itself is unit-tested here.

use crate::CliError;
use agora_sched::model::{
    CardDepArgs, CardLinkArgs, CardUpdateArgs, EscalateArgs, LeaseArgs, LeaseKind, LinkRel,
    PoolArgs, QuietArgs,
};
use agora_sched::policy::TeamPolicy;
use agora_sched::timer;

/// The longest quiet window `quiet open` accepts: one day.
const QUIET_MAX_SECS: u64 = 86_400;
/// The same bound in milliseconds, for `--until`.
const QUIET_MAX_MS: i64 = 86_400_000;

/// `card update` flags. At least one field must change; value and size are finite and not negative.
pub(super) fn card_update_args(
    priority: Option<i64>,
    value: Option<f64>,
    size: Option<f64>,
    team: Option<String>,
    needs_owner: Option<bool>,
    owner_action: Option<String>,
) -> Result<CardUpdateArgs, CliError> {
    let nothing_to_change = priority.is_none()
        && value.is_none()
        && size.is_none()
        && team.is_none()
        && needs_owner.is_none()
        && owner_action.is_none();
    if nothing_to_change {
        return Err(CliError::Usage(
            "card update needs at least one field to change: --priority, --value, --size, --team, \
             --needs-owner or --owner-action"
                .into(),
        ));
    }
    if let Some(v) = value {
        number("--value", v, v >= 0.0, "a number of at least 0")?;
    }
    if let Some(v) = size {
        number("--size", v, v >= 0.0, "a number of at least 0")?;
    }
    Ok(CardUpdateArgs {
        priority,
        value,
        size,
        team,
        needs_owner,
        owner_action,
    })
}

/// `card dep ID AFTER`: a card cannot wait for itself.
pub(super) fn card_dep_args(
    card: &str,
    after: String,
    remove: bool,
) -> Result<CardDepArgs, CliError> {
    non_empty("the dependency card", &after)?;
    if after == card {
        return Err(CliError::Usage("a card cannot wait for itself".into()));
    }
    Ok(CardDepArgs { after, remove })
}

/// `card link ID REL TARGET`: REL is one of the `LinkRel` names.
pub(super) fn card_link_args(
    rel: &str,
    target: String,
    remove: bool,
) -> Result<CardLinkArgs, CliError> {
    let rel = LinkRel::parse(rel).ok_or_else(|| {
        CliError::Usage(format!(
            "rel must be one of re, blocks, relates, duplicates, caused_by, job, message or note, not {rel:?}"
        ))
    })?;
    non_empty("the link target", &target)?;
    Ok(CardLinkArgs {
        rel,
        target,
        remove,
    })
}

/// `card escalate ID --owner-action TEXT [--to KEY…]`: the action must say something, and no key may be empty.
pub(super) fn escalate_args(
    owner_action: String,
    to: Vec<String>,
) -> Result<EscalateArgs, CliError> {
    if owner_action.trim().is_empty() {
        return Err(CliError::Usage(
            "--owner-action must say what the owner has to do".into(),
        ));
    }
    if to.iter().any(String::is_empty) {
        return Err(CliError::Usage(
            "--to takes public keys; an empty key is not one".into(),
        ));
    }
    Ok(EscalateArgs { owner_action, to })
}

/// `pool define` flags: a non-empty name and a positive capacity.
pub(super) fn pool_args(
    name: String,
    capacity: f64,
    unit: String,
    note: String,
) -> Result<PoolArgs, CliError> {
    non_empty("the pool name", &name)?;
    let capacity = number("--capacity", capacity, capacity > 0.0, "a positive number")?;
    Ok(PoolArgs {
        name,
        capacity,
        unit,
        note,
    })
}

/// `pool lease`: exactly one of `--pool` (which takes `amount` units) or `--path`. A path lease has no amount,
/// so it is sent as 0, which the daemon ignores; the client canonicalises the path before signing.
pub(super) fn lease_args(
    pool: Option<String>,
    path: Option<String>,
    amount: f64,
    ttl_secs: Option<f64>,
    card: Option<String>,
    task: Option<String>,
    note: String,
) -> Result<LeaseArgs, CliError> {
    let ttl_s = ttl(ttl_secs)?;
    let (kind, resource, amount) = match (pool, path) {
        (Some(name), None) => {
            non_empty("the pool name", &name)?;
            let amount = number("--amount", amount, amount > 0.0, "a positive number")?;
            (LeaseKind::Pool, name, amount)
        }
        (None, Some(path)) => {
            non_empty("--path", &path)?;
            (LeaseKind::Path, path, 0.0)
        }
        _ => {
            return Err(CliError::Usage(
                "a lease needs exactly one of --pool or --path".into(),
            ));
        }
    };
    Ok(LeaseArgs {
        kind,
        resource,
        amount,
        ttl_s,
        card,
        task,
        note,
    })
}

/// `quiet open`: exactly one of `--for-secs N` (the window ends N seconds after `now`) or `--until TIME` (it ends at
/// TIME, parsed by `timer::parse_at`). Either way the window lasts between one second and one day.
pub(super) fn quiet_args(
    now: i64,
    for_secs: Option<u64>,
    until: Option<&str>,
    note: String,
) -> Result<QuietArgs, CliError> {
    let expected_end_ms = match (for_secs, until) {
        (Some(secs), None) => {
            if !(1..=QUIET_MAX_SECS).contains(&secs) {
                return Err(CliError::Usage(format!(
                    "a quiet window lasts between 1 and {QUIET_MAX_SECS} seconds, not {secs}"
                )));
            }
            i64::try_from(secs)
                .ok()
                .and_then(|s| s.checked_mul(1_000))
                .and_then(|ms| now.checked_add(ms))
                .ok_or_else(quiet_end_overflow)?
        }
        (None, Some(text)) => {
            let end = timer::parse_at(text).map_err(CliError::Usage)?;
            let length_ms = end.checked_sub(now);
            if !length_ms.is_some_and(|ms| (1_000..=QUIET_MAX_MS).contains(&ms)) {
                return Err(CliError::Usage(format!(
                    "--until must be between 1 second and 1 day from now, not {text}"
                )));
            }
            end
        }
        _ => {
            return Err(CliError::Usage(
                "quiet open needs exactly one of --for-secs or --until".into(),
            ));
        }
    };
    Ok(QuietArgs {
        expected_end_ms,
        note,
    })
}

fn quiet_end_overflow() -> CliError {
    CliError::Usage("the quiet window ends beyond the largest representable time".into())
}

/// The `policy set` flags as a `TeamPolicy`; `None` keeps inheriting. Thresholds are seconds, never negative
/// or infinite, and an escalation key may not be empty.
pub(super) fn team_policy(p: TeamPolicy) -> Result<TeamPolicy, CliError> {
    for (flag, value) in [
        ("--stale-after", p.stale_after_s),
        ("--escalate-after", p.escalate_after_s),
        ("--reclaim-after", p.reclaim_after_s),
    ] {
        if let Some(v) = value {
            number(flag, v, v >= 0.0, "a number of seconds, at least 0")?;
        }
    }
    if p.escalation_chain
        .as_ref()
        .is_some_and(|chain| chain.iter().any(String::is_empty))
    {
        return Err(CliError::Usage(
            "--escalation-chain takes public keys; an empty key is not one".into(),
        ));
    }
    Ok(p)
}

/// `--kind` of `pool leases`: absent means every kind.
pub(super) fn lease_kind(kind: Option<&str>) -> Result<Option<LeaseKind>, CliError> {
    kind.map(|k| {
        LeaseKind::parse(k).ok_or_else(|| {
            CliError::Usage(format!("--kind must be path, pool or quiet, not {k:?}"))
        })
    })
    .transpose()
}

/// `--ttl-secs` of a lease: absent keeps the daemon's default; present must be positive.
pub(super) fn ttl(ttl_secs: Option<f64>) -> Result<Option<f64>, CliError> {
    ttl_secs
        .map(|s| number("--ttl-secs", s, s > 0.0, "a positive number of seconds"))
        .transpose()
}

/// A numeric flag: finite and satisfying `ok`, else a usage error that names the flag and the rule.
pub(super) fn number(flag: &str, value: f64, ok: bool, rule: &str) -> Result<f64, CliError> {
    if value.is_finite() && ok {
        Ok(value)
    } else {
        Err(CliError::Usage(format!(
            "{flag} must be {rule}, not {value}"
        )))
    }
}

pub(super) fn non_empty(what: &str, value: &str) -> Result<(), CliError> {
    if value.is_empty() {
        Err(CliError::Usage(format!("{what} must not be empty")))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
