//! card *, sprint *, note *.

use crate::client::Client;
use crate::ops::{now_ms, sanitize, sign_op};
use crate::{ClientError, Ctx, Out};
use agora_proto::{NoteWire, Reply, Req};
use agora_store::SignedNote;
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct CardCreateParams {
    pub id: String,
    pub title: String,
    pub lane: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CardMoveParams {
    pub id: String,
    /// backlog ready claimed doing review done blocked ...
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct CardEvidenceParams {
    pub id: String,
    pub reference: String,
}

#[derive(Debug, Clone)]
pub struct CardApproveParams {
    pub id: String,
}

#[derive(Debug, Clone)]
pub struct CardReclaimParams {
    pub id: String,
}

#[derive(Debug, Clone)]
pub struct CardListParams {
    pub state: Option<String>,
    pub lane: Option<String>,
    pub limit: i64,
}

#[derive(Debug, Clone)]
pub struct SprintCreateParams {
    pub id: String,
    pub name: String,
    pub goal: String,
    /// Start, ms since epoch.
    pub starts: i64,
    /// End, ms since epoch (must be after start).
    pub ends: i64,
}

#[derive(Debug, Clone)]
pub struct SprintAddParams {
    pub sprint: String,
    pub card: String,
}

#[derive(Debug, Clone)]
pub struct SprintStartParams {
    pub sprint: String,
}

#[derive(Debug, Clone)]
pub struct SprintCloseParams {
    pub sprint: String,
}

#[derive(Debug, Clone)]
pub struct SprintSummaryParams {
    pub sprint: String,
}

#[derive(Debug, Clone)]
pub struct NotePublishParams {
    pub id: String,
    pub title: String,
    pub body: String,
    pub tags: String,
    pub supersedes: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NoteSearchParams {
    pub query: String,
    pub limit: i64,
}

pub async fn card_create(ctx: &Ctx, p: CardCreateParams) -> Result<Out, ClientError> {
    let CardCreateParams { id, title, lane } = p;
    let arg = match lane {
        Some(lane) => serde_json::json!({"title": title, "lane": lane}),
        None => serde_json::json!({"title": title}),
    };
    let op = sign_op(&ctx.identity, "card.create", &id, &arg.to_string());
    send(ctx, Req::CardCreate { op }).await?;
    Ok(done(&id, format!("created card {id}")))
}

pub async fn card_move(ctx: &Ctx, p: CardMoveParams) -> Result<Out, ClientError> {
    let CardMoveParams { id, state } = p;
    let op = sign_op(&ctx.identity, "card.move", &id, &state);
    send(ctx, Req::CardMove { op }).await?;
    Ok(done(&id, format!("moved card {id} to {state}")))
}

pub async fn card_evidence(ctx: &Ctx, p: CardEvidenceParams) -> Result<Out, ClientError> {
    let CardEvidenceParams { id, reference } = p;
    let op = sign_op(&ctx.identity, "card.evidence", &id, &reference);
    send(ctx, Req::CardEvidence { op }).await?;
    Ok(done(&id, format!("added evidence to card {id}")))
}

pub async fn card_approve(ctx: &Ctx, p: CardApproveParams) -> Result<Out, ClientError> {
    let CardApproveParams { id } = p;
    let op = sign_op(&ctx.identity, "card.approve", &id, "");
    send(ctx, Req::CardApprove { op }).await?;
    Ok(done(&id, format!("approved card {id}")))
}

pub async fn card_reclaim(ctx: &Ctx, p: CardReclaimParams) -> Result<Out, ClientError> {
    let CardReclaimParams { id } = p;
    let op = sign_op(&ctx.identity, "card.reclaim", &id, "");
    send(ctx, Req::CardReclaim { op }).await?;
    Ok(done(&id, format!("reclaimed card {id}")))
}

pub async fn card_list(ctx: &Ctx, p: CardListParams) -> Result<Out, ClientError> {
    let CardListParams { state, lane, limit } = p;
    let mut client = Client::connect(ctx).await?;
    let Reply::Cards { cards } = client.call(Req::CardList { state, lane, limit }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    let text = cards
        .iter()
        .map(|v| {
            format!(
                "{}  {}  {}  {}",
                sanitize(&v.id),
                sanitize(&v.state),
                sanitize(&v.lane),
                sanitize(&v.title)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Out {
        json: serde_json::json!({ "cards": to_json(&cards)? }),
        text,
    })
}

pub async fn sprint_create(ctx: &Ctx, p: SprintCreateParams) -> Result<Out, ClientError> {
    let SprintCreateParams {
        id,
        name,
        goal,
        starts,
        ends,
    } = p;
    let arg = serde_json::json!({"name": name, "goal": goal, "starts": starts, "ends": ends});
    let op = sign_op(&ctx.identity, "sprint.create", &id, &arg.to_string());
    send(ctx, Req::SprintCreate { op }).await?;
    Ok(done(&id, format!("created sprint {id}")))
}

pub async fn sprint_add(ctx: &Ctx, p: SprintAddParams) -> Result<Out, ClientError> {
    let SprintAddParams { sprint, card } = p;
    let op = sign_op(&ctx.identity, "sprint.add", &sprint, &card);
    send(ctx, Req::SprintAdd { op }).await?;
    Ok(done(
        &sprint,
        format!("added card {card} to sprint {sprint}"),
    ))
}

pub async fn sprint_start(ctx: &Ctx, p: SprintStartParams) -> Result<Out, ClientError> {
    let SprintStartParams { sprint } = p;
    let op = sign_op(&ctx.identity, "sprint.start", &sprint, "");
    send(ctx, Req::SprintStart { op }).await?;
    Ok(done(&sprint, format!("started sprint {sprint}")))
}

pub async fn sprint_close(ctx: &Ctx, p: SprintCloseParams) -> Result<Out, ClientError> {
    let SprintCloseParams { sprint } = p;
    let op = sign_op(&ctx.identity, "sprint.close", &sprint, "");
    send(ctx, Req::SprintClose { op }).await?;
    Ok(done(&sprint, format!("closed sprint {sprint}")))
}

pub async fn sprint_summary(ctx: &Ctx, p: SprintSummaryParams) -> Result<Out, ClientError> {
    let SprintSummaryParams { sprint } = p;
    let mut client = Client::connect(ctx).await?;
    let Reply::Sprint { summary } = client
        .call(Req::SprintSummary {
            sprint: sprint.clone(),
        })
        .await?
    else {
        return Err(ClientError::UnexpectedReply);
    };
    let text = format!(
        "sprint {sprint}: total {}, done {}, in flight {}, blocked {}",
        summary.total, summary.done, summary.in_flight, summary.blocked
    );
    Ok(Out {
        json: serde_json::json!({ "summary": to_json(&summary)? }),
        text,
    })
}

pub async fn note_publish(ctx: &Ctx, p: NotePublishParams) -> Result<Out, ClientError> {
    let NotePublishParams {
        id,
        title,
        body,
        tags,
        supersedes,
    } = p;
    let note = SignedNote {
        id: id.clone(),
        author: [0; 32],
        ts: now_ms(),
        title,
        body,
        tags,
        supersedes,
        sig: vec![],
    }
    .sign(&ctx.identity);
    send(
        ctx,
        Req::NotePublish {
            note: NoteWire::from_note(&note),
        },
    )
    .await?;
    Ok(done(&id, format!("published note {id}")))
}

pub async fn note_search(ctx: &Ctx, p: NoteSearchParams) -> Result<Out, ClientError> {
    let NoteSearchParams { query, limit } = p;
    let mut client = Client::connect(ctx).await?;
    let Reply::Notes { notes } = client.call(Req::NoteSearch { query, limit }).await? else {
        return Err(ClientError::UnexpectedReply);
    };
    let text = notes
        .iter()
        .map(|v| format!("{}  {}", sanitize(&v.id), sanitize(&v.title)))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Out {
        json: serde_json::json!({ "notes": to_json(&notes)? }),
        text,
    })
}

/// Connect, send one signed mutation, and expect `Reply::Unit`.
async fn send(ctx: &Ctx, req: Req) -> Result<(), ClientError> {
    let mut client = Client::connect(ctx).await?;
    match client.call(req).await? {
        Reply::Unit => Ok(()),
        _ => Err(ClientError::UnexpectedReply),
    }
}

/// The `--json` result of a mutation.
fn done(id: &str, text: String) -> Out {
    Out {
        json: serde_json::json!({ "ok": true, "id": id }),
        text,
    }
}

fn to_json<T: Serialize>(value: &T) -> Result<serde_json::Value, ClientError> {
    serde_json::to_value(value).map_err(|e| ClientError::Proto(e.into()))
}
