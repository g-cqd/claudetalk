//! Cards, sprints, knowledge notes (see SPEC "Handlers: board").

use crate::dispatch::{daemon_error, op_from_wire, proto_error, signed_by_conn, unrouted};
use crate::{Conn, Shared, now_ms};
use agora_core::CardState;
use agora_proto::{CardView, ErrorBody, Event, NoteHit, Reply, Req, SprintSummaryView};
use agora_store::{Store, StoreError};

/// Handles Card*, Sprint*, NotePublish, NoteSearch; anything else -> `Err(dispatch::unrouted())`.
pub async fn handle(sh: &Shared, conn: &Conn, req: Req) -> Result<Reply, ErrorBody> {
    match req {
        Req::CardCreate { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, subject) = (now_ms(), op.subject.clone());
            commit(sh, subject, card_changed, move |s| s.create_card(now, &op)).await
        }
        Req::CardMove { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, review) = (now_ms(), sh.config.require_review);
            let subject = op.subject.clone();
            commit(sh, subject, card_changed, move |s| {
                s.transition(now, &op, review)
            })
            .await
        }
        Req::CardEvidence { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, subject) = (now_ms(), op.subject.clone());
            commit(sh, subject, card_changed, move |s| s.add_evidence(now, &op)).await
        }
        Req::CardApprove { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, subject) = (now_ms(), op.subject.clone());
            commit(sh, subject, card_changed, move |s| s.approve(now, &op)).await
        }
        Req::CardReclaim { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, subject) = (now_ms(), op.subject.clone());
            commit(sh, subject, card_changed, move |s| {
                s.reclaim_stale(now, &op)
            })
            .await
        }
        Req::CardList { state, lane, limit } => {
            let pk = conn.pubkey;
            let cards = sh
                .reader
                .exec(move |s| {
                    s.role_of(&pk)?;
                    let want = state
                        .map(|v| {
                            CardState::parse(&v).ok_or(StoreError::Invalid("unknown card state"))
                        })
                        .transpose()?;
                    s.list_cards(want, lane.as_deref(), limit)
                })
                .await
                .map_err(daemon_error)?;
            Ok(Reply::Cards {
                cards: cards.iter().map(CardView::from_row).collect(),
            })
        }
        Req::SprintCreate { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, subject) = (now_ms(), op.subject.clone());
            commit(sh, subject, sprint_changed, move |s| {
                s.create_sprint(now, &op)
            })
            .await
        }
        Req::SprintAdd { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, subject) = (now_ms(), op.subject.clone());
            commit(sh, subject, sprint_changed, move |s| {
                s.add_to_sprint(now, &op)
            })
            .await
        }
        Req::SprintStart { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, subject) = (now_ms(), op.subject.clone());
            commit(sh, subject, sprint_changed, move |s| {
                s.start_sprint(now, &op)
            })
            .await
        }
        Req::SprintClose { op } => {
            let op = op_from_wire(conn, &op)?;
            let (now, subject) = (now_ms(), op.subject.clone());
            commit(sh, subject, sprint_changed, move |s| {
                s.close_sprint(now, &op)
            })
            .await
        }
        Req::SprintSummary { sprint } => {
            let pk = conn.pubkey;
            let summary = sh
                .reader
                .exec(move |s| {
                    s.role_of(&pk)?;
                    s.sprint_summary(&sprint)
                })
                .await
                .map_err(daemon_error)?;
            Ok(Reply::Sprint {
                summary: SprintSummaryView::from_summary(&summary),
            })
        }
        Req::NotePublish { note } => {
            let note = note.to_note().map_err(|e| proto_error(&e))?;
            signed_by_conn(conn, &note.author)?;
            let (now, id) = (now_ms(), note.id.clone());
            commit(sh, id, note_published, move |s| s.publish_note(now, &note)).await
        }
        Req::NoteSearch { query, limit } => {
            let pk = conn.pubkey;
            let hits = sh
                .reader
                .exec(move |s| {
                    s.role_of(&pk)?;
                    s.search_notes(&query, limit)
                })
                .await
                .map_err(daemon_error)?;
            Ok(Reply::Notes {
                notes: hits
                    .into_iter()
                    .map(|(id, title)| NoteHit { id, title })
                    .collect(),
            })
        }
        _ => Err(unrouted()),
    }
}

/// Runs one write on the writer thread and answers `Unit`. The writer publishes
/// `event(subject)` after the commit, before the reply is delivered.
async fn commit<F>(
    sh: &Shared,
    subject: String,
    event: fn(String) -> Event,
    write: F,
) -> Result<Reply, ErrorBody>
where
    F: FnOnce(&mut Store) -> Result<(), StoreError> + Send + 'static,
{
    sh.writer
        .exec(move |s| {
            write(s)?;
            Ok(((), vec![event(subject)]))
        })
        .await
        .map_err(daemon_error)?;
    Ok(Reply::Unit)
}

fn card_changed(card: String) -> Event {
    Event::CardChanged { card }
}

fn sprint_changed(sprint: String) -> Event {
    Event::SprintChanged { sprint }
}

fn note_published(id: String) -> Event {
    Event::NotePublished { id }
}
