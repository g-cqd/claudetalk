//! timer *, pool *, lease *, quiet *, policy *, card update/dep/link/escalate/ext, watchdog, metrics, notices.
//! Text: one line per record, every untrusted string through `printable()` (see `sched/text.rs`).

mod text;

use crate::client::Client;
use crate::ops::sign_op;
use crate::{ClientError, Ctx, Out};
use agora_proto::{Reply, Req};
use agora_sched::claims::canonical_path;
use agora_sched::model::{
    BoardExtras, CardDepArgs, CardLinkArgs, CardUpdateArgs, EscalateArgs, LeaseArgs, LeaseKind,
    LeaseRenewArgs, PolicyArgs, PoolArgs, QuietArgs, TimerArgs,
};
use agora_sched::pyfmt::printable;
use serde::Serialize;
use std::ffi::OsString;
use std::path::PathBuf;

pub async fn timer_create(ctx: &Ctx, a: TimerArgs) -> Result<Out, ClientError> {
    let subject = a.id.clone().unwrap_or_default();
    let op = sign_op(&ctx.identity, "timer.create", &subject, &arg_json(&a)?);
    let Reply::Timer { timer } = call(ctx, Req::TimerCreate { op }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out("timer", &timer, text::timer_line(&timer))
}

pub async fn timer_delete(ctx: &Ctx, id: &str) -> Result<Out, ClientError> {
    let op = sign_op(&ctx.identity, "timer.delete", id, "{}");
    call_unit(ctx, Req::TimerDelete { op }).await?;
    Ok(done(id, format!("deleted timer {}", printable(id))))
}

pub async fn timers(ctx: &Ctx, owner: Option<String>) -> Result<Out, ClientError> {
    let Reply::Timers { timers } = call(ctx, Req::TimerList { owner }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out(
        "timers",
        &timers,
        text::join(timers.iter().map(text::timer_line)),
    )
}

pub async fn pool_define(ctx: &Ctx, a: PoolArgs) -> Result<Out, ClientError> {
    let op = sign_op(&ctx.identity, "pool.define", &a.name, &arg_json(&a)?);
    let Reply::Pool { pool } = call(ctx, Req::PoolDefine { op }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out("pool", &pool, text::pool_line(&pool))
}

pub async fn pools(ctx: &Ctx) -> Result<Out, ClientError> {
    let Reply::Pools { pools } = call(ctx, Req::PoolList).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out(
        "pools",
        &pools,
        text::join(pools.iter().map(text::pool_line)),
    )
}

/// A Path lease's resource is canonicalised here, against the current directory, before it is signed.
pub async fn lease(ctx: &Ctx, a: LeaseArgs) -> Result<Out, ClientError> {
    let a = if a.kind == LeaseKind::Path {
        let cwd = current_dir()?;
        LeaseArgs {
            resource: canonical_resource(&a.resource, &cwd)?,
            ..a
        }
    } else {
        a
    };
    let op = sign_op(&ctx.identity, "lease.acquire", "", &arg_json(&a)?);
    let Reply::Lease { lease } = call(ctx, Req::LeaseAcquire { op }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out("lease", &lease, text::lease_line(&lease))
}

pub async fn lease_renew(ctx: &Ctx, id: &str, ttl_s: Option<f64>) -> Result<Out, ClientError> {
    let arg = arg_json(&LeaseRenewArgs { ttl_s })?;
    let op = sign_op(&ctx.identity, "lease.renew", id, &arg);
    let Reply::Lease { lease } = call(ctx, Req::LeaseRenew { op }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out("lease", &lease, text::lease_line(&lease))
}

pub async fn lease_release(ctx: &Ctx, id: &str) -> Result<Out, ClientError> {
    let op = sign_op(&ctx.identity, "lease.release", id, "{}");
    call_unit(ctx, Req::LeaseRelease { op }).await?;
    Ok(done(id, format!("released lease {}", printable(id))))
}

pub async fn leases(
    ctx: &Ctx,
    kind: Option<LeaseKind>,
    holder: Option<String>,
    active_only: bool,
) -> Result<Out, ClientError> {
    let req = Req::LeaseList {
        kind,
        holder,
        active_only,
    };
    let Reply::Leases { leases } = call(ctx, req).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out(
        "leases",
        &leases,
        text::join(leases.iter().map(text::lease_line)),
    )
}

pub async fn quiet_open(ctx: &Ctx, a: QuietArgs) -> Result<Out, ClientError> {
    let op = sign_op(&ctx.identity, "quiet.open", "", &arg_json(&a)?);
    let Reply::Lease { lease } = call(ctx, Req::QuietOpen { op }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out("lease", &lease, text::lease_line(&lease))
}

pub async fn quiet_close(ctx: &Ctx, id: &str) -> Result<Out, ClientError> {
    let op = sign_op(&ctx.identity, "quiet.close", id, "{}");
    call_unit(ctx, Req::QuietClose { op }).await?;
    Ok(done(id, format!("closed quiet window {}", printable(id))))
}

pub async fn policy_set(ctx: &Ctx, a: PolicyArgs) -> Result<Out, ClientError> {
    let subject = format!("{}/{}", a.team, a.lane);
    let op = sign_op(&ctx.identity, "policy.set", &subject, &arg_json(&a)?);
    let Reply::Policy { policy } = call(ctx, Req::PolicySet { op }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out("policy", &policy, text::policy_line(&policy))
}

pub async fn policies(ctx: &Ctx) -> Result<Out, ClientError> {
    let Reply::Policies { policies } = call(ctx, Req::PolicyList).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out(
        "policies",
        &policies,
        text::join(policies.iter().map(text::policy_line)),
    )
}

/// The reply is the card's extension record, returned under `cards` as a one-element list.
pub async fn card_update(ctx: &Ctx, card: &str, a: CardUpdateArgs) -> Result<Out, ClientError> {
    let op = sign_op(&ctx.identity, "card.update", card, &arg_json(&a)?);
    let Reply::CardExts { cards } = call(ctx, Req::CardUpdate { op }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out(
        "cards",
        &cards,
        text::join(cards.iter().map(text::card_ext_line)),
    )
}

pub async fn card_dep(ctx: &Ctx, card: &str, a: CardDepArgs) -> Result<Out, ClientError> {
    let verb = if a.remove { "removed" } else { "added" };
    let preposition = if a.remove { "from" } else { "to" };
    let op = sign_op(&ctx.identity, "card.dep", card, &arg_json(&a)?);
    call_unit(ctx, Req::CardDep { op }).await?;
    let text = format!(
        "{verb} dependency {} {preposition} card {}",
        printable(&a.after),
        printable(card)
    );
    Ok(done(card, text))
}

pub async fn card_link(ctx: &Ctx, card: &str, a: CardLinkArgs) -> Result<Out, ClientError> {
    let verb = if a.remove { "unlinked" } else { "linked" };
    let text = format!(
        "{verb} card {} {} {}",
        printable(card),
        a.rel.as_str(),
        printable(&a.target)
    );
    let op = sign_op(&ctx.identity, "card.link", card, &arg_json(&a)?);
    call_unit(ctx, Req::CardLink { op }).await?;
    Ok(done(card, text))
}

pub async fn escalate(ctx: &Ctx, card: &str, a: EscalateArgs) -> Result<Out, ClientError> {
    let op = sign_op(&ctx.identity, "card.escalate", card, &arg_json(&a)?);
    let Reply::Escalated { notified } = call(ctx, Req::CardEscalate { op }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out("notified", &notified, text::escalated_text(card, &notified))
}

pub async fn card_ext(ctx: &Ctx, ids: Vec<String>) -> Result<Out, ClientError> {
    let Reply::CardExts { cards } = call(ctx, Req::CardExt { ids }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out(
        "cards",
        &cards,
        text::join(cards.iter().map(text::card_ext_line)),
    )
}

pub async fn watchdog(ctx: &Ctx) -> Result<Out, ClientError> {
    let Reply::Watchdog { report } = call(ctx, Req::WatchdogReport).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out("report", &report, text::watchdog_text(&report))
}

pub async fn metrics(
    ctx: &Ctx,
    team: Option<String>,
    lane: Option<String>,
    since_ms: Option<i64>,
) -> Result<Out, ClientError> {
    let Reply::Metrics { report } = call(
        ctx,
        Req::Metrics {
            team,
            lane,
            since_ms,
        },
    )
    .await?
    else {
        return Err(ClientError::UnexpectedReply);
    };
    out("report", &report, text::metrics_text(&report))
}

/// Typed: the board's scheduling extras, for the board summary.
pub async fn board_extras(ctx: &Ctx) -> Result<BoardExtras, ClientError> {
    let Reply::BoardExtras { extras } = call(ctx, Req::BoardExtras).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    Ok(extras)
}

pub async fn notices(ctx: &Ctx, after: i64, limit: u32) -> Result<Out, ClientError> {
    let Reply::Notices { notices } = call(ctx, Req::Notices { after, limit }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    out(
        "notices",
        &notices,
        text::join(notices.iter().map(text::notice_line)),
    )
}

/// Connect and send one request.
async fn call(ctx: &Ctx, req: Req) -> Result<Reply, ClientError> {
    let mut client = Client::connect(ctx).await?;
    client.call(req).await
}

/// Connect, send one signed mutation, and expect `Reply::Unit`.
async fn call_unit(ctx: &Ctx, req: Req) -> Result<(), ClientError> {
    match call(ctx, req).await? {
        Reply::Unit => Ok(()),
        _ => Err(ClientError::UnexpectedReply),
    }
}

/// The `--json` result of a list or record reply: `{key: value}`, with `text` for the terminal.
fn out<T: Serialize + ?Sized>(key: &str, value: &T, text: String) -> Result<Out, ClientError> {
    Ok(Out {
        json: keyed(key, to_json(value)?),
        text,
    })
}

/// The `--json` result of a mutation that answers `Reply::Unit`.
fn done(id: &str, text: String) -> Out {
    Out {
        json: serde_json::json!({ "ok": true, "id": id }),
        text,
    }
}

/// `{key: value}` as a JSON object.
fn keyed(key: &str, value: serde_json::Value) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert(key.to_string(), value);
    serde_json::Value::Object(map)
}

/// The compact JSON of an `*Args` struct: the `arg` of a signed op.
fn arg_json<T: Serialize>(a: &T) -> Result<String, ClientError> {
    serde_json::to_string(a).map_err(|e| ClientError::Proto(e.into()))
}

fn to_json<T: Serialize + ?Sized>(value: &T) -> Result<serde_json::Value, ClientError> {
    serde_json::to_value(value).map_err(|e| ClientError::Proto(e.into()))
}

/// The current directory as a string; a Path lease's relative resource is resolved against it.
fn current_dir() -> Result<String, ClientError> {
    std::env::current_dir()?
        .into_os_string()
        .into_string()
        .map_err(|_| ClientError::Usage("the current directory is not valid UTF-8".to_string()))
}

/// Canonical form of a Path lease's resource, the way work.rs canonicalises files: the path is made absolute
/// against `cwd` and normalised (`canonical_path` also rejects an empty path or one with a NUL), then the longest
/// existing prefix is resolved with `std::fs::canonicalize` and the rest is appended. A path that does not exist
/// yet therefore still names the same place as the same path later, and symlinks in the existing part are resolved.
fn canonical_resource(resource: &str, cwd: &str) -> Result<String, ClientError> {
    let absolute = canonical_path(resource, cwd, None).map_err(ClientError::Usage)?;
    let mut existing = PathBuf::from(&absolute);
    let mut tail: Vec<OsString> = Vec::new();
    let mut real = loop {
        match std::fs::canonicalize(&existing) {
            Ok(real) => break real,
            Err(_) => match (existing.file_name(), existing.parent()) {
                (Some(name), Some(parent)) => {
                    tail.push(name.to_os_string());
                    existing = parent.to_path_buf();
                }
                _ => {
                    return Err(ClientError::Usage(format!(
                        "{}: cannot resolve this path",
                        printable(resource)
                    )));
                }
            },
        }
    };
    for part in tail.iter().rev() {
        real.push(part);
    }
    real.into_os_string()
        .into_string()
        .map_err(|_| ClientError::Usage(format!("{}: not valid UTF-8", printable(resource))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A temp directory whose real path is the canonical root (macOS puts temp dirs behind a symlink).
    fn root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().canonicalize().expect("canonical root");
        (dir, root)
    }

    fn as_str(p: &std::path::Path) -> &str {
        p.to_str().expect("utf-8 temp path")
    }

    #[test]
    fn done_is_ok_true_with_the_id() {
        let out = done("t1", "deleted timer t1".to_string());
        assert_eq!(out.json, json!({"ok": true, "id": "t1"}));
        assert_eq!(out.text, "deleted timer t1");
    }

    #[test]
    fn keyed_wraps_the_value_under_its_key() {
        assert_eq!(keyed("timers", json!([1, 2])), json!({"timers": [1, 2]}));
    }

    #[test]
    fn canonical_resource_resolves_a_relative_existing_file_under_the_cwd() {
        let (_dir, root) = root();
        std::fs::write(root.join("notes.txt"), b"x").expect("write");
        let got = canonical_resource("notes.txt", as_str(&root)).expect("canonical");
        assert_eq!(got, as_str(&root.join("notes.txt")));
    }

    #[test]
    fn canonical_resource_appends_the_missing_tail_to_the_existing_ancestor() {
        let (_dir, root) = root();
        let got = canonical_resource("missing/deeper/file.rs", as_str(&root)).expect("canonical");
        assert_eq!(got, as_str(&root.join("missing/deeper/file.rs")));
    }

    #[test]
    fn canonical_resource_resolves_a_symlink_in_the_existing_prefix() {
        let (_dir, root) = root();
        std::fs::create_dir(root.join("real")).expect("mkdir");
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).expect("symlink");
        let got = canonical_resource("link/file.rs", as_str(&root)).expect("canonical");
        assert_eq!(got, as_str(&root.join("real/file.rs")));
    }

    #[test]
    fn canonical_resource_normalises_dot_parts_and_keeps_an_absolute_path() {
        let (_dir, root) = root();
        let got = canonical_resource("/./a/../b", as_str(&root)).expect("canonical");
        assert_eq!(got, "/b");
    }

    #[test]
    fn canonical_resource_refuses_an_empty_or_nul_path() {
        let (_dir, root) = root();
        assert!(matches!(
            canonical_resource("", as_str(&root)),
            Err(ClientError::Usage(_))
        ));
        assert!(matches!(
            canonical_resource("a\0b", as_str(&root)),
            Err(ClientError::Usage(_))
        ));
    }

    #[test]
    fn arg_json_is_the_compact_json_of_the_args_struct() {
        let arg = arg_json(&QuietArgs {
            expected_end_ms: 5,
            note: String::new(),
        })
        .expect("serialises");
        assert_eq!(arg, r#"{"expected_end_ms":5,"note":""}"#);
    }
}
