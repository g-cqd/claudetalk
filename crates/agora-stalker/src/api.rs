//! JSON API: one GET route per read verb of `agora_client::api`, plus composed
//! views (overview) and read-only file inspection (delivery heartbeats,
//! capabilities, daemon log). Handlers return the API's own `Out::json`, so the
//! web app shows exactly what the CLI's `--json` prints.

use crate::files;
use agora_client::api::{board, chat, org, sched, status, work};
use agora_client::error::ClientError;
use agora_client::{Ctx, Out};
use agora_sched::model::{LeaseKind, TaskFilter, TaskState};
use agora_sched::order::OrderPolicy;
use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// A query string is a flat map; helpers pull typed fields out of it.
type Q = HashMap<String, String>;

/// Parse a query string without the axum `query` feature: split on `&`, then the
/// first `=`, percent-decode and turn `+` into a space.
pub(crate) fn parse_query(uri: &Uri) -> Q {
    let mut q = Q::new();
    let Some(raw) = uri.query() else {
        return q;
    };
    for pair in raw.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        q.insert(decode(k), decode(v));
    }
    q
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn q_str(q: &Q, key: &str) -> Option<String> {
    q.get(key)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn q_i64(q: &Q, key: &str, default: i64) -> i64 {
    q.get(key)
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

fn q_u32(q: &Q, key: &str, default: u32) -> u32 {
    q.get(key)
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

/// An `Out` as a JSON response.
fn ok(out: Out) -> Response {
    Json(out.json).into_response()
}

/// A `ClientError` as a JSON response. `Remote` refusals (the daemon said no)
/// are 502 so the app can tell them apart from watcher-side failures.
fn err(e: ClientError) -> Response {
    let status = match e {
        ClientError::Remote(_) => StatusCode::BAD_GATEWAY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(json!({ "error": e.to_string() }))).into_response()
}

fn wrap(result: Result<Out, ClientError>) -> Response {
    match result {
        Ok(out) => ok(out),
        Err(e) => err(e),
    }
}

/// A section of the overview that may fail without failing the whole page.
async fn soft(fut: impl std::future::Future<Output = Result<Out, ClientError>>) -> Value {
    match fut.await {
        Ok(out) => out.json,
        Err(e) => json!({ "error": e.to_string() }),
    }
}

async fn whoami(State(ctx): State<Arc<Ctx>>) -> Response {
    wrap(org::whoami(&ctx).await)
}

async fn overview(State(ctx): State<Arc<Ctx>>) -> Response {
    let whoami = org::whoami(&ctx).await;
    Json(json!({
        "whoami": match whoami { Ok(o) => o.json, Err(e) => json!({"error": e.to_string()}) },
        "work": soft(work::status(&ctx, None)).await,
        "notices": soft(sched::notices(&ctx, 0, 20)).await,
        "metrics": soft(sched::metrics(&ctx, None, None, None)).await,
        "watchdog": soft(sched::watchdog(&ctx)).await,
        "cards": soft(board::card_list(&ctx, board::CardListParams {
            state: None, lane: None, limit: 500,
        })).await,
        "tasks": soft(work::list(&ctx, TaskFilter { limit: 500, ..TaskFilter::default() })).await,
        "files": files::stats(&ctx.paths),
    }))
    .into_response()
}

/// The oversight read of `verb`, falling back to the own-view `fallback` for a
/// `Forbidden` (an identity below supervisor). The result carries `scope`:
/// `"all"` or `"own"`, so the page can say which one it is showing.
async fn all_or_own(
    verb: impl std::future::Future<Output = Result<Out, ClientError>>,
    fallback: impl std::future::Future<Output = Result<Out, ClientError>>,
) -> Result<Out, ClientError> {
    match verb.await {
        Ok(mut out) => {
            if let Value::Object(map) = &mut out.json {
                map.insert("scope".into(), Value::String("all".into()));
            }
            Ok(out)
        }
        Err(ClientError::Remote(ref e)) if is_forbidden(e) => {
            let mut out = fallback.await?;
            if let Value::Object(map) = &mut out.json {
                map.insert("scope".into(), Value::String("own".into()));
            }
            Ok(out)
        }
        Err(e) => Err(e),
    }
}

/// The daemon answers a role refusal with `Forbidden: ...`.
fn is_forbidden(e: &agora_proto::ErrorBody) -> bool {
    matches!(e.code, agora_proto::ErrorCode::Forbidden)
}

async fn channels(State(ctx): State<Arc<Ctx>>) -> Response {
    let verb = org::all_channels(&ctx);
    let fallback = org::channel_list(&ctx);
    wrap(all_or_own(verb, fallback).await)
}

async fn channel_info(State(ctx): State<Arc<Ctx>>, Path(id): Path<String>) -> Response {
    wrap(org::channel_info(&ctx, org::ChannelInfoParams { channel: id }).await)
}

async fn messages(State(ctx): State<Arc<Ctx>>, Path(id): Path<String>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    let p = chat::ReadParams {
        channel: id,
        after: q_i64(&q, "after", 0),
        limit: q_i64(&q, "limit", 50).min(500),
    };
    let verb = chat::read_any(&ctx, p.clone());
    let fallback = chat::read(&ctx, p);
    wrap(all_or_own(verb, fallback).await)
}

/// Every status with a supervisor identity, else the own view.
async fn statuses(State(ctx): State<Arc<Ctx>>) -> Response {
    let verb = status::all_statuses(&ctx);
    let fallback = status::statuses(&ctx);
    wrap(all_or_own(verb, fallback).await)
}

async fn inbox(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    wrap(
        chat::inbox(
            &ctx,
            chat::InboxParams {
                wait: None,
                after: q_i64(&q, "after", 0),
                limit: q_i64(&q, "limit", 100).min(500),
            },
        )
        .await,
    )
}

async fn cards(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    wrap(
        board::card_list(
            &ctx,
            board::CardListParams {
                state: q_str(&q, "state"),
                lane: q_str(&q, "lane"),
                limit: q_i64(&q, "limit", 200).min(1000),
            },
        )
        .await,
    )
}

async fn sprint(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    let Some(sprint) = q_str(&q, "sprint") else {
        return err(ClientError::Usage(
            "sprint id is required: ?sprint=<id>".into(),
        ));
    };
    wrap(board::sprint_summary(&ctx, board::SprintSummaryParams { sprint }).await)
}

async fn notes(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    wrap(
        board::note_search(
            &ctx,
            board::NoteSearchParams {
                query: q_str(&q, "q").unwrap_or_default(),
                limit: q_i64(&q, "limit", 50).min(500),
            },
        )
        .await,
    )
}

fn task_filter(q: &Q) -> Result<TaskFilter, ClientError> {
    let mut f = TaskFilter {
        scope: q_str(q, "scope"),
        ..TaskFilter::default()
    };
    if let Some(states) = q_str(q, "states") {
        for s in states.split(',') {
            let s = s.trim();
            if s.is_empty() {
                continue;
            }
            let Some(state) = TaskState::parse(s) else {
                return Err(ClientError::Usage(format!("unknown task state {s:?}")));
            };
            f.states.push(state);
        }
    }
    if let Some(ids) = q_str(q, "ids") {
        f.ids = ids
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
    }
    f.limit = q_u32(q, "limit", 200).min(1000);
    Ok(f)
}

async fn work_status(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    wrap(work::status(&ctx, q_str(&q, "scope")).await)
}

async fn work_tasks(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    match task_filter(&q) {
        Ok(f) => wrap(work::list(&ctx, f).await),
        Err(e) => err(e),
    }
}

async fn work_task(State(ctx): State<Arc<Ctx>>, Path(id): Path<String>) -> Response {
    wrap(work::show(&ctx, &id).await)
}

async fn work_log(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    wrap(
        work::log(
            &ctx,
            q_str(&q, "task"),
            q_str(&q, "actor"),
            q_str(&q, "scope"),
            q_u32(&q, "limit", 100).min(1000),
        )
        .await,
    )
}

async fn work_scopes(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    wrap(work::scopes(&ctx, q_str(&q, "root")).await)
}

async fn work_settings(State(ctx): State<Arc<Ctx>>) -> Response {
    wrap(work::settings(&ctx, None).await)
}

async fn work_plan(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    let policy = match q_str(&q, "policy") {
        None => OrderPolicy::default(),
        Some(s) => match OrderPolicy::parse(&s) {
            Some(p) => p,
            None => return err(ClientError::Usage(format!("unknown policy {s:?}"))),
        },
    };
    wrap(
        work::plan(
            &ctx,
            q_str(&q, "scope"),
            policy,
            q_u32(&q, "agents", 8).min(256),
            q_str(&q, "assign").is_some_and(|a| a == "1" || a == "true"),
            None,
        )
        .await,
    )
}

async fn timers(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    wrap(sched::timers(&ctx, q_str(&q, "owner")).await)
}

async fn pools(State(ctx): State<Arc<Ctx>>) -> Response {
    wrap(sched::pools(&ctx).await)
}

async fn leases(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    let kind = match q_str(&q, "kind") {
        None => None,
        Some(s) => match s.as_str() {
            "path" => Some(LeaseKind::Path),
            "pool" => Some(LeaseKind::Pool),
            "quiet" => Some(LeaseKind::Quiet),
            _ => return err(ClientError::Usage(format!("unknown lease kind {s:?}"))),
        },
    };
    wrap(sched::leases(&ctx, kind, q_str(&q, "holder"), q_str(&q, "all").is_none()).await)
}

async fn policies(State(ctx): State<Arc<Ctx>>) -> Response {
    wrap(sched::policies(&ctx).await)
}

async fn watchdog(State(ctx): State<Arc<Ctx>>) -> Response {
    wrap(sched::watchdog(&ctx).await)
}

async fn metrics(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    let since = q_str(&q, "since_ms").and_then(|s| s.parse().ok());
    wrap(sched::metrics(&ctx, q_str(&q, "team"), q_str(&q, "lane"), since).await)
}

async fn notices(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    wrap(sched::notices(&ctx, q_i64(&q, "after", 0), q_u32(&q, "limit", 50).min(500)).await)
}

async fn index() -> Response {
    axum::http::Response::builder()
        .header("content-type", "text/html; charset=utf-8")
        .body(include_str!("index.html").to_owned())
        .unwrap()
        .into_response()
}

/// `/metrics`: Prometheus text format (version 0.0.4), built from the same
/// read verbs the page uses, so Grafana/Prometheus can scrape Agora. A read
/// that fails skips its series instead of failing the scrape; the daemon-down
/// case still answers, with `agora_up 0`.
async fn prometheus(State(ctx): State<Arc<Ctx>>) -> Response {
    let mut out = String::new();
    let stats = files::stats(&ctx.paths);
    let up = stats
        .get("daemon_up")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    out.push_str("# TYPE agora_up gauge\nagora_up ");
    out.push_str(if up { "1\n" } else { "0\n" });

    let card_states = board::card_list(
        &ctx,
        board::CardListParams {
            state: None,
            lane: None,
            limit: 500,
        },
    )
    .await;
    out.push_str("# TYPE agora_cards gauge\n");
    match card_states {
        Ok(o) => {
            let mut counts: std::collections::BTreeMap<String, i64> = Default::default();
            if let Some(cards) = o.json.get("cards").and_then(Value::as_array) {
                for c in cards {
                    if let Some(state) = c.get("state").and_then(Value::as_str) {
                        *counts.entry(state.to_string()).or_default() += 1;
                    }
                }
            }
            for (state, n) in counts {
                out.push_str(&format!(
                    "agora_cards{{state=\"{}\"}} {n}\n",
                    escape_label(&state)
                ));
            }
        }
        Err(_) => out.push_str("# agora_cards unavailable\n"),
    }

    if let Ok(w) = work::status(&ctx, None).await {
        let report = w.json.get("report");
        out.push_str("# TYPE agora_work_tasks gauge\n");
        if let Some(counts) = report
            .and_then(|r| r.get("counts"))
            .and_then(Value::as_object)
        {
            for (state, n) in counts {
                if let Some(n) = n.as_i64() {
                    out.push_str(&format!(
                        "agora_work_tasks{{state=\"{}\"}} {n}\n",
                        escape_label(state)
                    ));
                }
            }
        }
        if let Some(m) = report.and_then(|r| r.get("machine")) {
            for (name, key, mul) in [
                ("agora_machine_memory_used_kib", "memory_used_kib", 1.0),
                ("agora_machine_memory_budget_kib", "memory_budget_kib", 1.0),
                ("agora_machine_load", "load", 1.0),
                ("agora_machine_idle_pct", "idle", 1.0),
                ("agora_machine_jobs", "jobs", 1.0),
            ] {
                if let Some(v) = m.get(key).and_then(Value::as_f64) {
                    out.push_str(&format!("# TYPE {name} gauge\n{name} {}\n", v * mul));
                }
            }
        }
    }

    let status_read = all_or_own(status::all_statuses(&ctx), status::statuses(&ctx)).await;
    out.push_str("# TYPE agora_statuses gauge\n");
    match status_read {
        Ok(o) => {
            let mut counts: std::collections::BTreeMap<String, i64> = Default::default();
            let mut online = 0i64;
            if let Some(rows) = o.json.get("statuses").and_then(Value::as_array) {
                for s in rows {
                    if let Some(state) = s.get("state").and_then(Value::as_str) {
                        *counts.entry(state.to_string()).or_default() += 1;
                    }
                    if s.get("online").and_then(Value::as_bool).unwrap_or(false) {
                        online += 1;
                    }
                }
            }
            for (state, n) in counts {
                out.push_str(&format!(
                    "agora_statuses{{state=\"{}\"}} {n}\n",
                    escape_label(&state)
                ));
            }
            out.push_str(&format!(
                "# TYPE agora_online gauge\nagora_online {online}\n"
            ));
        }
        Err(_) => out.push_str("# agora_statuses unavailable\n"),
    }

    match all_or_own(org::all_channels(&ctx), org::channel_list(&ctx)).await {
        Ok(o) => {
            let n = o
                .json
                .get("channels")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            out.push_str(&format!(
                "# TYPE agora_channels gauge\nagora_channels {n}\n"
            ));
        }
        Err(_) => out.push_str("# agora_channels unavailable\n"),
    }

    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
        .into_response()
}

/// A Prometheus label value: backslash, quote and newline escaped.
fn escape_label(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// `/api/export/messages?after=N`: every message with global seq > N, merged
/// across all channels in seq order, one JSON object per line (NDJSON). The
/// ingestion feed for Splunk/Elasticsearch: pull with the last seq as the next
/// `after`. Supervisor identity: all channels; worker: its own. Encrypted
/// bodies the identity cannot open export as `null` text.
async fn export_messages(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    const MAX_LINES: usize = 1000;
    let q = parse_query(&uri);
    let after = q_i64(&q, "after", 0);
    let chans = match all_or_own(org::all_channels(&ctx), org::channel_list(&ctx)).await {
        Ok(o) => o,
        Err(e) => return err(e),
    };
    let mut merged: Vec<Value> = Vec::new();
    if let Some(channels) = chans.json.get("channels").and_then(Value::as_array) {
        for c in channels {
            let id = match c.get("id").and_then(Value::as_str) {
                Some(id) => id.to_string(),
                None => continue,
            };
            let read = chat::read_any(
                &ctx,
                chat::ReadParams {
                    channel: id.clone(),
                    after,
                    limit: 500,
                },
            )
            .await;
            let Ok(o) = read else { continue };
            if let Some(msgs) = o.json.get("messages").and_then(Value::as_array) {
                for m in msgs {
                    if let Value::Object(mut row) = m.clone() {
                        row.insert("channel".into(), Value::String(id.clone()));
                        merged.push(Value::Object(row));
                    }
                }
            }
        }
    }
    merged.retain(|m| m.get("seq").and_then(Value::as_i64).unwrap_or(0) > after);
    merged.sort_by_key(|m| m.get("seq").and_then(Value::as_i64).unwrap_or(0));
    merged.dedup_by_key(|m| m.get("seq").and_then(Value::as_i64).unwrap_or(0));
    merged.truncate(MAX_LINES);
    let body = merged
        .iter()
        .map(|m| m.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let body = if body.is_empty() { body } else { body + "\n" };
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/x-ndjson; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// The whole router. Every route is a GET; nothing writes.
pub fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/metrics", get(prometheus))
        .route("/api/export/messages", get(export_messages))
        .route("/api/whoami", get(whoami))
        .route("/api/overview", get(overview))
        .route("/api/channels", get(channels))
        .route("/api/channels/{id}/info", get(channel_info))
        .route("/api/channels/{id}/messages", get(messages))
        .route("/api/statuses", get(statuses))
        .route("/api/inbox", get(inbox))
        .route("/api/board/cards", get(cards))
        .route("/api/board/sprint", get(sprint))
        .route("/api/board/notes", get(notes))
        .route("/api/work/status", get(work_status))
        .route("/api/work/tasks", get(work_tasks))
        .route("/api/work/tasks/{id}", get(work_task))
        .route("/api/work/log", get(work_log))
        .route("/api/work/scopes", get(work_scopes))
        .route("/api/work/settings", get(work_settings))
        .route("/api/work/plan", get(work_plan))
        .route("/api/sched/timers", get(timers))
        .route("/api/sched/pools", get(pools))
        .route("/api/sched/leases", get(leases))
        .route("/api/sched/policies", get(policies))
        .route("/api/sched/watchdog", get(watchdog))
        .route("/api/sched/metrics", get(metrics))
        .route("/api/sched/notices", get(notices))
        .route("/api/files/capabilities", get(files::capabilities))
        .route("/api/files/heartbeats", get(files::heartbeats))
        .route("/api/files/daemon-log", get(files::daemon_log))
        .route("/api/files/stats", get(files::stats_route))
        .with_state(ctx)
}
