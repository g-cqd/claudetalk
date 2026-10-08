//! card *, sprint *, note *: clap args -> `agora_client::api::board` -> Out.

use crate::{CardCmd, CliError, Ctx, NoteCmd, Out, SprintCmd};
use agora_client::api::board::{
    self as api, CardApproveParams, CardCreateParams, CardEvidenceParams, CardListParams,
    CardMoveParams, CardReclaimParams, NotePublishParams, NoteSearchParams, SprintAddParams,
    SprintCloseParams, SprintCreateParams, SprintStartParams, SprintSummaryParams,
};

pub async fn card(ctx: &Ctx, c: CardCmd) -> Result<Out, CliError> {
    match c {
        CardCmd::Create {
            id,
            title,
            lane,
            priority,
            value,
            size,
            team,
            after,
        } => {
            let p = CardCreateParams {
                id,
                title,
                lane,
                priority,
                value,
                size,
                team,
                after,
            };
            api::card_create(ctx, p).await
        }
        c @ (CardCmd::Update { .. }
        | CardCmd::Dep { .. }
        | CardCmd::Link { .. }
        | CardCmd::Escalate { .. }) => crate::cmd_sched::card(ctx, c).await,
        CardCmd::Move { id, state } => api::card_move(ctx, CardMoveParams { id, state }).await,
        CardCmd::Evidence { id, reference } => {
            api::card_evidence(ctx, CardEvidenceParams { id, reference }).await
        }
        CardCmd::Approve { id } => api::card_approve(ctx, CardApproveParams { id }).await,
        CardCmd::Reclaim { id } => api::card_reclaim(ctx, CardReclaimParams { id }).await,
        CardCmd::List { state, lane, limit } => {
            api::card_list(ctx, CardListParams { state, lane, limit }).await
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
            api::sprint_create(
                ctx,
                SprintCreateParams {
                    id,
                    name,
                    goal,
                    starts,
                    ends,
                },
            )
            .await
        }
        SprintCmd::Add { sprint, card } => {
            api::sprint_add(ctx, SprintAddParams { sprint, card }).await
        }
        SprintCmd::Start { sprint } => api::sprint_start(ctx, SprintStartParams { sprint }).await,
        SprintCmd::Close { sprint } => api::sprint_close(ctx, SprintCloseParams { sprint }).await,
        SprintCmd::Summary { sprint } => {
            api::sprint_summary(ctx, SprintSummaryParams { sprint }).await
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
            api::note_publish(
                ctx,
                NotePublishParams {
                    id,
                    title,
                    body,
                    tags,
                    supersedes,
                },
            )
            .await
        }
        NoteCmd::Search { query, limit } => {
            api::note_search(ctx, NoteSearchParams { query, limit }).await
        }
    }
}
