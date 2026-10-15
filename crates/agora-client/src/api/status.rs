//! status set, status clear, statuses.

use crate::client::Client;
use crate::ops::{sanitize, sign_op};
use crate::{ClientError, Ctx, Out};
use agora_proto::{Reply, Req, StatusView};
use agora_store::hex;
use serde_json::{Map, Value};

#[derive(Debug, Clone)]
pub struct StatusSetParams {
    /// Short free text, e.g. online | busy | idle | away.
    pub state: String,
    /// Harness name, e.g. codex, claude, vibe, cli. One row per (identity,
    /// harness), so two harnesses that share a folder key keep their own row.
    pub harness: Option<String>,
    /// The folder the caller works in.
    pub folder: Option<String>,
    /// What the caller is working on.
    pub matter: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StatusClearParams {
    /// The harness row to clear; `None` clears the row with no harness.
    pub harness: Option<String>,
}

/// Set the caller's status. The subject is the caller's own hex key.
pub async fn status_set(ctx: &Ctx, p: StatusSetParams) -> Result<Out, ClientError> {
    let mut arg = Map::new();
    arg.insert("state".to_owned(), Value::String(p.state.clone()));
    if let Some(harness) = &p.harness {
        arg.insert("harness".to_owned(), Value::String(harness.clone()));
    }
    if let Some(folder) = &p.folder {
        arg.insert("folder".to_owned(), Value::String(folder.clone()));
    }
    if let Some(matter) = &p.matter {
        arg.insert("matter".to_owned(), Value::String(matter.clone()));
    }
    let arg = Value::Object(arg);
    let op = sign_op(
        &ctx.identity,
        "status.set",
        &hex(&ctx.identity.public()),
        &arg.to_string(),
    );
    let mut client = Client::connect(ctx).await?;
    client.call(Req::StatusSet { op }).await?;
    let text = format!(
        "status {}{}",
        sanitize(&p.state),
        p.harness
            .as_deref()
            .map(|h| format!(" on {h}"))
            .unwrap_or_default()
    );
    Ok(Out { json: arg, text })
}

/// Clear the caller's status for one harness.
pub async fn status_clear(ctx: &Ctx, p: StatusClearParams) -> Result<Out, ClientError> {
    let arg = match &p.harness {
        Some(harness) => serde_json::json!({ "harness": harness }),
        None => serde_json::json!({}),
    };
    let op = sign_op(
        &ctx.identity,
        "status.clear",
        &hex(&ctx.identity.public()),
        &arg.to_string(),
    );
    let mut client = Client::connect(ctx).await?;
    client.call(Req::StatusClear { op }).await?;
    let text = match &p.harness {
        Some(harness) => format!("cleared status on {harness}"),
        None => "cleared status".to_string(),
    };
    Ok(Out {
        json: serde_json::json!({ "cleared": true }),
        text,
    })
}

/// The caller's visible statuses: its own plus those of its channel peers,
/// most recent first, each with the daemon's online flag.
pub async fn statuses(ctx: &Ctx) -> Result<Out, ClientError> {
    let mut client = Client::connect(ctx).await?;
    let Reply::Statuses { statuses } = client.call(Req::Statuses).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    let text = status_text(&statuses);
    Ok(Out {
        json: serde_json::json!({ "statuses": statuses }),
        text,
    })
}

/// Supervisor or above: every status row, most recent first, with the online
/// flags (the stalker's oversight read). Refused for a worker.
pub async fn all_statuses(ctx: &Ctx) -> Result<Out, ClientError> {
    let mut client = Client::connect(ctx).await?;
    let Reply::Statuses { statuses } = client.call(Req::AllStatuses).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    let text = status_text(&statuses);
    Ok(Out {
        json: serde_json::json!({ "statuses": statuses, "scope": "all" }),
        text,
    })
}

/// One line per row: `pseudonym  harness  state  online  folder  matter`.
/// Every field is other agents' text except the online flag, so the caller
/// must treat the whole line as untrusted data.
fn status_text(statuses: &[StatusView]) -> String {
    statuses
        .iter()
        .map(|v| {
            format!(
                "{}  {}  {}  {}  {}  {}",
                sanitize(&v.pseudonym),
                sanitize(&v.harness),
                sanitize(&v.state),
                if v.online { "online" } else { "offline" },
                sanitize(&v.folder),
                sanitize(&v.matter),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
