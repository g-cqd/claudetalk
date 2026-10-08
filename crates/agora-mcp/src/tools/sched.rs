//! Tool handlers: work_schedule, pool_define, pool_lease, pool_release, quiet_window, watchdog_report, reclaim,
//! escalate, policy_set, metrics, card_update. They call `agora_client::api::sched` and `api::work`; text that
//! echoes other agents' content goes through `framed_output`. A timer with a command and a path lease are refused
//! over HTTP, like filesystem paths in M3.

use super::work_args::*;
use super::{ToolCx, ToolError, ToolResult, framed_output, output};
use agora_client::api::{board as board_api, sched as api, work as work_api};
use agora_client::ops::now_ms;
use agora_sched::model::{
    AddArgs, CardDepArgs, CardLinkArgs, CardUpdateArgs as CardUpdateOp, EscalateArgs as EscalateOp,
    LeaseArgs, LeaseKind, LinkRel, PolicyArgs, PoolArgs, QuietArgs, TimerArgs,
};
use agora_sched::policy::TeamPolicy;
use agora_sched::timer;

/// The longest manual quiet window the tool opens, in seconds (one day).
const MAX_QUIET_SECS: u64 = 86_400;

/// A required argument: the usage error names it when it is missing.
fn need(value: Option<String>, what: &str) -> Result<String, ToolError> {
    value.ok_or_else(|| ToolError::usage(format!("{what} is required")))
}

/// A feature that needs a local filesystem or a local command is refused over HTTP (`allow_paths` is false there).
fn require_stdio(allow_paths: bool, what: &str) -> Result<(), ToolError> {
    if allow_paths {
        Ok(())
    } else {
        Err(ToolError::usage(format!(
            "{what} is not available over HTTP"
        )))
    }
}

/// The `TimerArgs` of a create: the spec from `when`, the id as given, and the task template.
fn schedule_create(a: &WorkScheduleArgs) -> Result<TimerArgs, ToolError> {
    let when = a.when.as_deref().ok_or_else(|| {
        ToolError::usage("create needs when: at:<time>, every:<N>[s|m|h|d] or cron:<5 fields>")
    })?;
    let spec = timer::parse_spec(when).map_err(ToolError::usage)?;
    Ok(TimerArgs {
        id: a.id.clone(),
        spec,
        template: timer_template(a),
    })
}

/// The task a timer queues: the template fields that were given, the `AddArgs` defaults for the others. The id is
/// left unset: the timer names each task `<timer-id>-<n>`.
fn timer_template(a: &WorkScheduleArgs) -> AddArgs {
    let d = AddArgs::default();
    AddArgs {
        id: None,
        title: a.title.clone().unwrap_or_default(),
        kind: a.kind.clone().unwrap_or(d.kind),
        scope: a.scope.clone(),
        after: a.after.clone().unwrap_or_default(),
        size: a.size.unwrap_or(d.size),
        value: a.value.unwrap_or(d.value),
        files: a.files.clone().unwrap_or_default(),
        weight: a.weight.unwrap_or(d.weight),
        command: a.command.clone(),
        cwd: a.cwd.clone(),
        card: a.card.clone(),
    }
}

/// Queue a timer (create), list the timers of an owner, or delete one.
pub async fn work_schedule(cx: &ToolCx<'_>, a: WorkScheduleArgs) -> ToolResult {
    match a.action.as_str() {
        "create" => {
            if a.command.is_some() {
                require_stdio(cx.engine.config.allow_paths, "a timer command")?;
            }
            let args = schedule_create(&a)?;
            Ok(output(api::timer_create(&cx.peer.ctx, args).await?))
        }
        "list" => Ok(framed_output(api::timers(&cx.peer.ctx, a.owner).await?)),
        "delete" => {
            let id = need(a.id, "id")?;
            Ok(output(api::timer_delete(&cx.peer.ctx, &id).await?))
        }
        _ => Err(ToolError::usage("action must be create, list or delete")),
    }
}

/// Define or resize a capacity pool.
pub async fn pool_define(cx: &ToolCx<'_>, a: PoolDefineArgs) -> ToolResult {
    let args = PoolArgs {
        name: a.name,
        capacity: a.capacity,
        unit: a.unit.unwrap_or_default(),
        note: a.note.unwrap_or_default(),
    };
    Ok(output(api::pool_define(&cx.peer.ctx, args).await?))
}

/// What `pool_lease` does: renew a lease, or acquire one.
#[derive(Debug, PartialEq)]
enum LeasePlan {
    Renew { id: String, ttl_s: Option<f64> },
    Acquire(LeaseArgs),
}

/// The target of a new lease: a number of units of a pool, or a path.
#[derive(Debug, PartialEq)]
enum LeaseTarget {
    Pool { name: String, amount: f64 },
    Path(String),
}

/// Exactly one of a pool (with its amount, default 1) or a path. Giving both, or neither, is a usage error, and so
/// is an amount with a path (a path lease holds the whole path).
fn lease_target(
    pool: Option<String>,
    path: Option<String>,
    amount: Option<f64>,
) -> Result<LeaseTarget, ToolError> {
    match (pool, path) {
        (Some(name), None) => Ok(LeaseTarget::Pool {
            name,
            amount: amount.unwrap_or(1.0),
        }),
        (None, Some(path)) => {
            if amount.is_some() {
                return Err(ToolError::usage("amount applies to pool leases only"));
            }
            Ok(LeaseTarget::Path(path))
        }
        (Some(_), Some(_)) => Err(ToolError::usage("give pool or path, not both")),
        (None, None) => Err(ToolError::usage(
            "give pool (with amount) or path to lease, or renew to renew a lease",
        )),
    }
}

/// `renew` takes a lease id and an optional ttl only; otherwise the target and the card, task and note are used.
fn lease_plan(a: PoolLeaseArgs) -> Result<LeasePlan, ToolError> {
    if let Some(id) = a.renew {
        if a.pool.is_some()
            || a.path.is_some()
            || a.amount.is_some()
            || a.card.is_some()
            || a.task.is_some()
            || a.note.is_some()
        {
            return Err(ToolError::usage("renew takes only a lease id and ttl_secs"));
        }
        return Ok(LeasePlan::Renew {
            id,
            ttl_s: a.ttl_secs,
        });
    }
    let (kind, resource, amount) = match lease_target(a.pool, a.path, a.amount)? {
        LeaseTarget::Pool { name, amount } => (LeaseKind::Pool, name, amount),
        LeaseTarget::Path(path) => (LeaseKind::Path, path, 0.0),
    };
    Ok(LeasePlan::Acquire(LeaseArgs {
        kind,
        resource,
        amount,
        ttl_s: a.ttl_secs,
        card: a.card,
        task: a.task,
        note: a.note.unwrap_or_default(),
    }))
}

/// Lease units of a pool or a path, or renew a lease. A path lease is refused over HTTP.
pub async fn pool_lease(cx: &ToolCx<'_>, a: PoolLeaseArgs) -> ToolResult {
    match lease_plan(a)? {
        LeasePlan::Renew { id, ttl_s } => {
            Ok(output(api::lease_renew(&cx.peer.ctx, &id, ttl_s).await?))
        }
        LeasePlan::Acquire(args) => {
            if args.kind == LeaseKind::Path {
                require_stdio(cx.engine.config.allow_paths, "a path lease")?;
            }
            Ok(output(api::lease(&cx.peer.ctx, args).await?))
        }
    }
}

/// Release a lease by its id.
pub async fn pool_release(cx: &ToolCx<'_>, a: PoolReleaseArgs) -> ToolResult {
    Ok(output(api::lease_release(&cx.peer.ctx, &a.lease).await?))
}

/// The `expected_end_ms` of a quiet window that lasts `for_secs` from `now`. The length is required and must be
/// 1 to 86400 seconds.
fn quiet_end_ms(now: i64, for_secs: Option<u64>) -> Result<i64, ToolError> {
    let secs = for_secs.ok_or_else(|| ToolError::usage("open needs for_secs (1 to 86400)"))?;
    if !(1..=MAX_QUIET_SECS).contains(&secs) {
        return Err(ToolError::usage("for_secs must be between 1 and 86400"));
    }
    i64::try_from(secs)
        .ok()
        .and_then(|s| s.checked_mul(1000))
        .and_then(|ms| now.checked_add(ms))
        .ok_or_else(|| ToolError::usage("for_secs is too large"))
}

/// Open a manual quiet window (controller), close one by its lease id, or show the open windows. The status text
/// echoes other agents' notes, so it is framed as untrusted data.
pub async fn quiet_window(cx: &ToolCx<'_>, a: QuietWindowArgs) -> ToolResult {
    match a.action.as_str() {
        "open" => {
            let args = QuietArgs {
                expected_end_ms: quiet_end_ms(now_ms(), a.for_secs)?,
                note: a.note.unwrap_or_default(),
            };
            Ok(output(api::quiet_open(&cx.peer.ctx, args).await?))
        }
        "close" => {
            let id = need(a.lease, "lease")?;
            Ok(output(api::quiet_close(&cx.peer.ctx, &id).await?))
        }
        "status" => Ok(framed_output(
            api::leases(&cx.peer.ctx, Some(LeaseKind::Quiet), None, true).await?,
        )),
        _ => Err(ToolError::usage("action must be open, close or status")),
    }
}

/// The machine, the running and the waiting jobs, and the stale cards and tasks, framed as untrusted data.
pub async fn watchdog_report(cx: &ToolCx<'_>, _a: WatchdogReportArgs) -> ToolResult {
    Ok(framed_output(api::watchdog(&cx.peer.ctx).await?))
}

/// The one thing a `reclaim` takes back.
#[derive(Debug, PartialEq)]
enum ReclaimTarget {
    Task(String),
    Lease(String),
    Card(String),
}

/// Exactly one of task, lease and card; none or several is a usage error.
fn reclaim_target(
    task: Option<String>,
    lease: Option<String>,
    card: Option<String>,
) -> Result<ReclaimTarget, ToolError> {
    match (task, lease, card) {
        (Some(id), None, None) => Ok(ReclaimTarget::Task(id)),
        (None, Some(id), None) => Ok(ReclaimTarget::Lease(id)),
        (None, None, Some(id)) => Ok(ReclaimTarget::Card(id)),
        _ => Err(ToolError::usage(
            "give exactly one of task, lease and card to reclaim",
        )),
    }
}

/// Take back a stale task or lease (through the work reclaim, `task:<id>` or `lease:<id>`), or a stale card (the
/// board's card reclaim, which takes no note).
pub async fn reclaim(cx: &ToolCx<'_>, a: ReclaimArgs) -> ToolResult {
    let note = a.note.unwrap_or_default();
    match reclaim_target(a.task, a.lease, a.card)? {
        ReclaimTarget::Task(id) => Ok(output(
            work_api::reclaim(&cx.peer.ctx, &format!("task:{id}"), &note).await?,
        )),
        ReclaimTarget::Lease(id) => Ok(output(
            work_api::reclaim(&cx.peer.ctx, &format!("lease:{id}"), &note).await?,
        )),
        ReclaimTarget::Card(id) => Ok(output(
            board_api::card_reclaim(&cx.peer.ctx, board_api::CardReclaimParams { id }).await?,
        )),
    }
}

/// Flag a card as needing its owner and notify the escalation chain (or the given keys).
pub async fn escalate(cx: &ToolCx<'_>, a: EscalateArgs) -> ToolResult {
    let op = EscalateOp {
        owner_action: a.owner_action,
        to: a.to.unwrap_or_default(),
    };
    Ok(output(api::escalate(&cx.peer.ctx, &a.card, op).await?))
}

/// The `PolicyArgs` of a set: the team and lane are required, the policy takes the fields that were given.
fn policy_args(a: &PolicySetArgs) -> Result<PolicyArgs, ToolError> {
    let team = need(a.team.clone(), "team")?;
    let lane = need(a.lane.clone(), "lane")?;
    Ok(PolicyArgs {
        team,
        lane,
        policy: TeamPolicy {
            stale_after_s: a.stale_after_s,
            escalate_after_s: a.escalate_after_s,
            reclaim_after_s: a.reclaim_after_s,
            reclaim: a.reclaim,
            require_review: a.require_review,
            wip_limit: a.wip_limit,
            escalation_chain: a.escalation_chain.clone(),
            sprint_rollover: a.sprint_rollover,
        },
    })
}

/// Set the policy of a team and lane, or list every policy (framed, since the policies echo other agents' text).
pub async fn policy_set(cx: &ToolCx<'_>, a: PolicySetArgs) -> ToolResult {
    if a.list == Some(true) {
        return Ok(framed_output(api::policies(&cx.peer.ctx).await?));
    }
    let args = policy_args(&a)?;
    Ok(output(api::policy_set(&cx.peer.ctx, args).await?))
}

/// Flow metrics for a team, a lane and a time, framed as untrusted data.
pub async fn metrics(cx: &ToolCx<'_>, a: MetricsArgs) -> ToolResult {
    Ok(framed_output(
        api::metrics(&cx.peer.ctx, a.team, a.lane, a.since_ms).await?,
    ))
}

/// One step of a `card_update`, applied in this order.
#[derive(Debug, PartialEq)]
enum CardOp {
    Fields(CardUpdateOp),
    Dep(CardDepArgs),
    Link(CardLinkArgs),
}

/// The steps of a `card_update`, validated before any is sent: the field changes, then the dependencies added and
/// then removed, then the link or unlink. Nothing to change is a usage error.
fn card_ops(a: &CardUpdateArgs) -> Result<Vec<CardOp>, ToolError> {
    let mut ops = Vec::new();
    if a.priority.is_some()
        || a.value.is_some()
        || a.size.is_some()
        || a.team.is_some()
        || a.needs_owner.is_some()
        || a.owner_action.is_some()
    {
        ops.push(CardOp::Fields(CardUpdateOp {
            priority: a.priority,
            value: a.value,
            size: a.size,
            team: a.team.clone(),
            needs_owner: a.needs_owner,
            owner_action: a.owner_action.clone(),
        }));
    }
    for after in a.after_add.iter().flatten() {
        ops.push(CardOp::Dep(CardDepArgs {
            after: after.clone(),
            remove: false,
        }));
    }
    for after in a.after_remove.iter().flatten() {
        ops.push(CardOp::Dep(CardDepArgs {
            after: after.clone(),
            remove: true,
        }));
    }
    match (&a.link_rel, &a.link_target) {
        (None, None) => {
            if a.unlink.is_some() {
                return Err(ToolError::usage("unlink needs link_rel and link_target"));
            }
        }
        (Some(rel), Some(target)) => {
            let rel = LinkRel::parse(rel).ok_or_else(|| {
                ToolError::usage(
                    "link_rel must be one of re, blocks, relates, duplicates, caused_by, job, message, note",
                )
            })?;
            ops.push(CardOp::Link(CardLinkArgs {
                rel,
                target: target.clone(),
                remove: a.unlink.unwrap_or(false),
            }));
        }
        _ => {
            return Err(ToolError::usage("link_rel and link_target go together"));
        }
    }
    if ops.is_empty() {
        return Err(ToolError::usage(
            "nothing to change: give a field, after_add, after_remove or link_rel with link_target",
        ));
    }
    Ok(ops)
}

/// Change card fields, dependencies and links, in that order, and return the card's final extension record.
pub async fn card_update(cx: &ToolCx<'_>, a: CardUpdateArgs) -> ToolResult {
    let ops = card_ops(&a)?;
    let card = a.card;
    for op in ops {
        match op {
            CardOp::Fields(args) => {
                api::card_update(&cx.peer.ctx, &card, args).await?;
            }
            CardOp::Dep(args) => {
                api::card_dep(&cx.peer.ctx, &card, args).await?;
            }
            CardOp::Link(args) => {
                api::card_link(&cx.peer.ctx, &card, args).await?;
            }
        }
    }
    Ok(framed_output(
        api::card_ext(&cx.peer.ctx, vec![card]).await?,
    ))
}

#[cfg(test)]
mod tests;
