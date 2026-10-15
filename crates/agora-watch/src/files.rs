//! Read-only file inspection: delivery heartbeats, the install capabilities
//! record, the daemon log and store sizes. Everything here only reads; the
//! daemon owns all writes.

use agora_daemon::Paths;
use axum::Json;
use axum::extract::State;
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agora_client::Ctx;

use crate::api::parse_query;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn obj_err(msg: impl Into<String>) -> Value {
    json!({ "error": msg.into() })
}

/// `GET /api/files/capabilities`: the JSON `agora install` records per client.
pub async fn capabilities(State(ctx): State<Arc<Ctx>>) -> Response {
    let path = ctx.paths.home.join("capabilities.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => Json(v).into_response(),
            Err(e) => Json(obj_err(format!("capabilities.json: {e}"))).into_response(),
        },
        Err(e) => Json(obj_err(format!("capabilities.json: {e}"))).into_response(),
    }
}

/// `GET /api/files/heartbeats`: every delivery heartbeat file under
/// `delivery/heartbeats/<pk>/<mechanism>`, with its timestamp and age.
pub async fn heartbeats(State(ctx): State<Arc<Ctx>>) -> Response {
    let now = now_ms();
    let root = ctx.paths.home.join("delivery").join("heartbeats");
    let mut beats = Vec::new();
    let mut pks = match std::fs::read_dir(&root) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect::<Vec<_>>(),
        Err(e) => {
            return Json(obj_err(format!("heartbeats: {e}"))).into_response();
        }
    };
    pks.sort_by_key(|e| e.file_name());
    for pk in pks {
        let Some(schemes) = std::fs::read_dir(pk.path()).ok() else {
            continue;
        };
        let mut schemes: Vec<_> = schemes.filter_map(|e| e.ok()).collect();
        schemes.sort_by_key(|e| e.file_name());
        for scheme in schemes {
            let raw = std::fs::read_to_string(scheme.path()).unwrap_or_default();
            let ts: i64 = raw.trim().parse().unwrap_or(0);
            beats.push(json!({
                "pk": pk.file_name().to_string_lossy(),
                "mechanism": scheme.file_name().to_string_lossy(),
                "ts_ms": ts,
                "age_s": if ts > 0 { (now - ts) / 1000 } else { -1 },
            }));
        }
    }
    Json(json!({ "now_ms": now, "heartbeats": beats })).into_response()
}

/// `GET /api/files/daemon-log?tail=200`: the last lines of the daemon log.
pub async fn daemon_log(State(ctx): State<Arc<Ctx>>, uri: Uri) -> Response {
    let q = parse_query(&uri);
    let tail: usize = q
        .get("tail")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(200)
        .min(5000);
    match std::fs::read_to_string(&ctx.paths.log) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect::<Vec<_>>();
            let start = lines.len().saturating_sub(tail);
            Json(json!({
                "total_lines": lines.len(),
                "lines": lines[start..],
            }))
            .into_response()
        }
        Err(e) => Json(obj_err(format!("daemon.log: {e}"))).into_response(),
    }
}

fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

fn dir_entry_count(path: &Path) -> usize {
    std::fs::read_dir(path)
        .map(|rd| rd.filter_map(|e| e.ok()).count())
        .unwrap_or(0)
}

/// Store sizes, blob and job counts, and whether the daemon answers on its
/// socket. `Value`, not a response, so the overview can embed it.
pub fn stats(paths: &Paths) -> Value {
    let daemon_up = std::os::unix::net::UnixStream::connect(&paths.socket).is_ok();
    json!({
        "home": paths.home.display().to_string(),
        "daemon_up": daemon_up,
        "db_bytes": file_size(&paths.db),
        "db_wal_bytes": file_size(&paths.home.join("agora.db-wal")),
        "blob_count": dir_entry_count(&paths.blobs),
        "job_count": dir_entry_count(&paths.home.join("work").join("jobs")),
        "job_log_count": dir_entry_count(&paths.home.join("work").join("logs")),
    })
}

/// `GET /api/files/stats`.
pub async fn stats_route(State(ctx): State<Arc<Ctx>>) -> Response {
    Json(stats(&ctx.paths)).into_response()
}
