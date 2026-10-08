//! pool, quiet, policy, watchdog, metrics, notices and the M5 `card` subcommands (update, dep, link,
//! escalate): clap args -> `agora_client::api::sched` -> Out.

use crate::{CardCmd, CliError, Ctx, Out, PolicyCmd, PoolCmd, QuietCmd};
use agora_client::api::sched as api;
use agora_client::ops::now_ms;
use agora_sched::model::{LeaseKind, PolicyArgs};
use agora_sched::policy::TeamPolicy;

mod args;

use args::{
    card_dep_args, card_link_args, card_update_args, escalate_args, lease_args, lease_kind,
    non_empty, pool_args, quiet_args, team_policy, ttl,
};

/// `card update|dep|link|escalate` (the other card commands are in `cmd_board`).
pub async fn card(ctx: &Ctx, c: CardCmd) -> Result<Out, CliError> {
    match c {
        CardCmd::Update {
            id,
            priority,
            value,
            size,
            team,
            needs_owner,
            owner_action,
        } => {
            let a = card_update_args(priority, value, size, team, needs_owner, owner_action)?;
            api::card_update(ctx, &id, a).await
        }
        CardCmd::Dep { id, after, remove } => {
            let a = card_dep_args(&id, after, remove)?;
            api::card_dep(ctx, &id, a).await
        }
        CardCmd::Link {
            id,
            rel,
            target,
            remove,
        } => {
            let a = card_link_args(&rel, target, remove)?;
            api::card_link(ctx, &id, a).await
        }
        CardCmd::Escalate {
            id,
            owner_action,
            to,
        } => {
            let a = escalate_args(owner_action, to)?;
            api::escalate(ctx, &id, a).await
        }
        _ => Err(CliError::Usage(
            "this card command is handled by the board, not the scheduler".into(),
        )),
    }
}

/// `pool define|list|lease|renew|release|leases`.
pub async fn pool(ctx: &Ctx, c: PoolCmd) -> Result<Out, CliError> {
    match c {
        PoolCmd::Define {
            name,
            capacity,
            unit,
            note,
        } => api::pool_define(ctx, pool_args(name, capacity, unit, note)?).await,
        PoolCmd::List => api::pools(ctx).await,
        PoolCmd::Lease {
            pool,
            amount,
            path,
            ttl_secs,
            card,
            task,
            note,
        } => {
            let a = lease_args(pool, path, amount, ttl_secs, card, task, note)?;
            api::lease(ctx, a).await
        }
        PoolCmd::Renew { lease, ttl_secs } => api::lease_renew(ctx, &lease, ttl(ttl_secs)?).await,
        PoolCmd::Release { lease } => api::lease_release(ctx, &lease).await,
        PoolCmd::Leases {
            kind,
            holder,
            active,
        } => api::leases(ctx, lease_kind(kind.as_deref())?, holder, active).await,
    }
}

/// `quiet open|close|status`: a quiet window is a lease of kind quiet.
pub async fn quiet(ctx: &Ctx, c: QuietCmd) -> Result<Out, CliError> {
    match c {
        QuietCmd::Open {
            for_secs,
            until,
            note,
        } => {
            let a = quiet_args(now_ms(), for_secs, until.as_deref(), note)?;
            api::quiet_open(ctx, a).await
        }
        QuietCmd::Close { lease } => api::quiet_close(ctx, &lease).await,
        QuietCmd::Status => api::leases(ctx, Some(LeaseKind::Quiet), None, true).await,
    }
}

/// `policy set|list`.
pub async fn policy(ctx: &Ctx, c: PolicyCmd) -> Result<Out, CliError> {
    match c {
        PolicyCmd::Set {
            team,
            lane,
            stale_after,
            escalate_after,
            reclaim_after,
            reclaim,
            require_review,
            wip_limit,
            escalation_chain,
            sprint_rollover,
        } => {
            non_empty("team", &team)?;
            non_empty("lane", &lane)?;
            let policy = team_policy(TeamPolicy {
                stale_after_s: stale_after,
                escalate_after_s: escalate_after,
                reclaim_after_s: reclaim_after,
                reclaim,
                require_review,
                wip_limit,
                escalation_chain,
                sprint_rollover,
            })?;
            api::policy_set(ctx, PolicyArgs { team, lane, policy }).await
        }
        PolicyCmd::List => api::policies(ctx).await,
    }
}

pub async fn watchdog(ctx: &Ctx) -> Result<Out, CliError> {
    api::watchdog(ctx).await
}

pub async fn metrics(
    ctx: &Ctx,
    team: Option<String>,
    lane: Option<String>,
    since_ms: Option<i64>,
) -> Result<Out, CliError> {
    api::metrics(ctx, team, lane, since_ms).await
}

pub async fn notices(ctx: &Ctx, after: i64, limit: u32) -> Result<Out, CliError> {
    api::notices(ctx, after, limit).await
}
