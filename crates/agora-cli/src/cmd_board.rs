//! card *, sprint *, note * (see SPEC "CLI commands").

use crate::client::Client;
use crate::ops::{now_ms, sanitize, sign_op};
use crate::{CardCmd, CliError, Ctx, NoteCmd, Out, SprintCmd};
use agora_proto::{NoteWire, Reply, Req};
use agora_store::SignedNote;
use serde::Serialize;

pub async fn card(ctx: &Ctx, c: CardCmd) -> Result<Out, CliError> {
    match c {
        CardCmd::Create { id, title, lane } => {
            let arg = match lane {
                Some(lane) => serde_json::json!({"title": title, "lane": lane}),
                None => serde_json::json!({"title": title}),
            };
            let op = sign_op(&ctx.identity, "card.create", &id, &arg.to_string());
            send(ctx, Req::CardCreate { op }).await?;
            Ok(done(&id, format!("created card {id}")))
        }
        CardCmd::Move { id, state } => {
            let op = sign_op(&ctx.identity, "card.move", &id, &state);
            send(ctx, Req::CardMove { op }).await?;
            Ok(done(&id, format!("moved card {id} to {state}")))
        }
        CardCmd::Evidence { id, reference } => {
            let op = sign_op(&ctx.identity, "card.evidence", &id, &reference);
            send(ctx, Req::CardEvidence { op }).await?;
            Ok(done(&id, format!("added evidence to card {id}")))
        }
        CardCmd::Approve { id } => {
            let op = sign_op(&ctx.identity, "card.approve", &id, "");
            send(ctx, Req::CardApprove { op }).await?;
            Ok(done(&id, format!("approved card {id}")))
        }
        CardCmd::Reclaim { id } => {
            let op = sign_op(&ctx.identity, "card.reclaim", &id, "");
            send(ctx, Req::CardReclaim { op }).await?;
            Ok(done(&id, format!("reclaimed card {id}")))
        }
        CardCmd::List { state, lane, limit } => {
            let mut client = Client::connect(ctx).await?;
            let Reply::Cards { cards } = client.call(Req::CardList { state, lane, limit }).await?
            else {
                return Err(CliError::UnexpectedReply);
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
    }
}

pub async fn sprint(ctx: &Ctx, c: SprintCmd) -> Result<Out, CliError> {
    match c {
        SprintCmd::Create {
            id,
            name,
            goal,
            starts,
            ends,
        } => {
            let arg =
                serde_json::json!({"name": name, "goal": goal, "starts": starts, "ends": ends});
            let op = sign_op(&ctx.identity, "sprint.create", &id, &arg.to_string());
            send(ctx, Req::SprintCreate { op }).await?;
            Ok(done(&id, format!("created sprint {id}")))
        }
        SprintCmd::Add { sprint, card } => {
            let op = sign_op(&ctx.identity, "sprint.add", &sprint, &card);
            send(ctx, Req::SprintAdd { op }).await?;
            Ok(done(
                &sprint,
                format!("added card {card} to sprint {sprint}"),
            ))
        }
        SprintCmd::Start { sprint } => {
            let op = sign_op(&ctx.identity, "sprint.start", &sprint, "");
            send(ctx, Req::SprintStart { op }).await?;
            Ok(done(&sprint, format!("started sprint {sprint}")))
        }
        SprintCmd::Close { sprint } => {
            let op = sign_op(&ctx.identity, "sprint.close", &sprint, "");
            send(ctx, Req::SprintClose { op }).await?;
            Ok(done(&sprint, format!("closed sprint {sprint}")))
        }
        SprintCmd::Summary { sprint } => {
            let mut client = Client::connect(ctx).await?;
            let Reply::Sprint { summary } = client
                .call(Req::SprintSummary {
                    sprint: sprint.clone(),
                })
                .await?
            else {
                return Err(CliError::UnexpectedReply);
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
    }
}

pub async fn note(ctx: &Ctx, c: NoteCmd) -> Result<Out, CliError> {
    match c {
        NoteCmd::Publish {
            id,
            title,
            body,
            tags,
            supersedes,
        } => {
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
        NoteCmd::Search { query, limit } => {
            let mut client = Client::connect(ctx).await?;
            let Reply::Notes { notes } = client.call(Req::NoteSearch { query, limit }).await?
            else {
                return Err(CliError::UnexpectedReply);
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
    }
}

/// Connect, send one signed mutation, and expect `Reply::Unit`.
async fn send(ctx: &Ctx, req: Req) -> Result<(), CliError> {
    let mut client = Client::connect(ctx).await?;
    match client.call(req).await? {
        Reply::Unit => Ok(()),
        _ => Err(CliError::UnexpectedReply),
    }
}

/// The `--json` result of a mutation.
fn done(id: &str, text: String) -> Out {
    Out {
        json: serde_json::json!({ "ok": true, "id": id }),
        text,
    }
}

fn to_json<T: Serialize>(value: &T) -> Result<serde_json::Value, CliError> {
    serde_json::to_value(value).map_err(|e| CliError::Proto(e.into()))
}
