//! Tool handlers: card_*, sprint_*, board_summary, note_*.

use super::args::*;
use super::{ToolCx, ToolResult, framed_output, output};
use agora_client::Out;
use agora_client::api::board as api;
use agora_client::api::sched as sched_api;
use agora_sched::model::BoardExtras;
use agora_sched::pyfmt::printable;
use serde_json::Value;
use std::collections::BTreeMap;

/// Clamp a list limit to the 1..=200 range every list tool accepts.
fn clamp_limit(limit: i64) -> i64 {
    limit.clamp(1, 200)
}

/// Create a card via `api::board::card_create`.
pub async fn card_create(cx: &ToolCx<'_>, a: CardCreateArgs) -> ToolResult {
    let p = api::CardCreateParams {
        id: a.id,
        title: a.title,
        lane: a.lane,
        priority: a.priority,
        value: a.value,
        size: a.size,
        team: a.team,
        after: a.after,
    };
    Ok(output(api::card_create(&cx.peer.ctx, p).await?))
}

/// Move a card to a state via `api::board::card_move`.
pub async fn card_move(cx: &ToolCx<'_>, a: CardMoveArgs) -> ToolResult {
    let p = api::CardMoveParams {
        id: a.id,
        state: a.state,
    };
    Ok(output(api::card_move(&cx.peer.ctx, p).await?))
}

/// Attach evidence (a reference) to a card via `api::board::card_evidence`.
pub async fn card_evidence(cx: &ToolCx<'_>, a: CardEvidenceArgs) -> ToolResult {
    let p = api::CardEvidenceParams {
        id: a.id,
        reference: a.reference,
    };
    Ok(output(api::card_evidence(&cx.peer.ctx, p).await?))
}

/// Approve a card in review via `api::board::card_approve`.
pub async fn card_approve(cx: &ToolCx<'_>, a: CardApproveArgs) -> ToolResult {
    let p = api::CardApproveParams { id: a.id };
    Ok(output(api::card_approve(&cx.peer.ctx, p).await?))
}

/// Reclaim a stuck card from its owner via `api::board::card_reclaim` (destructive).
pub async fn card_reclaim(cx: &ToolCx<'_>, a: CardReclaimArgs) -> ToolResult {
    let p = api::CardReclaimParams { id: a.id };
    Ok(output(api::card_reclaim(&cx.peer.ctx, p).await?))
}

/// List cards via `api::board::card_list` (limit default 100, clamped to 1..=200). Text: one line per card with its title, framed as untrusted data.
pub async fn card_list(cx: &ToolCx<'_>, a: CardListArgs) -> ToolResult {
    let p = api::CardListParams {
        state: a.state,
        lane: a.lane,
        limit: clamp_limit(a.limit),
    };
    Ok(framed_output(api::card_list(&cx.peer.ctx, p).await?))
}

/// Create a sprint via `api::board::sprint_create`.
pub async fn sprint_create(cx: &ToolCx<'_>, a: SprintCreateArgs) -> ToolResult {
    let p = api::SprintCreateParams {
        id: a.id,
        name: a.name,
        goal: a.goal,
        starts: a.starts,
        ends: a.ends,
    };
    Ok(output(api::sprint_create(&cx.peer.ctx, p).await?))
}

/// Add a card to a sprint via `api::board::sprint_add`.
pub async fn sprint_add(cx: &ToolCx<'_>, a: SprintAddArgs) -> ToolResult {
    let p = api::SprintAddParams {
        sprint: a.sprint,
        card: a.card,
    };
    Ok(output(api::sprint_add(&cx.peer.ctx, p).await?))
}

/// Start a sprint via `api::board::sprint_start`.
pub async fn sprint_start(cx: &ToolCx<'_>, a: SprintStartArgs) -> ToolResult {
    let p = api::SprintStartParams { sprint: a.sprint };
    Ok(output(api::sprint_start(&cx.peer.ctx, p).await?))
}

/// Close a sprint via `api::board::sprint_close`.
pub async fn sprint_close(cx: &ToolCx<'_>, a: SprintCloseArgs) -> ToolResult {
    let p = api::SprintCloseParams { sprint: a.sprint };
    Ok(output(api::sprint_close(&cx.peer.ctx, p).await?))
}

/// Sprint totals via `api::board::sprint_summary`.
pub async fn sprint_summary(cx: &ToolCx<'_>, a: SprintSummaryArgs) -> ToolResult {
    let p = api::SprintSummaryParams { sprint: a.sprint };
    Ok(output(api::sprint_summary(&cx.peer.ctx, p).await?))
}

/// `card_list` with limit 200 (and optional lane), reduced to json `{total, by_state:{state:n}, by_lane:{lane:n}, mine:[cards owned by me], blocked:[cards in state "blocked"], extras}` and a short human text summary. `extras` is the board's scheduling extras (`null` when the daemon cannot give them); its text lines follow the summary line. The text carries card titles, so it is framed as untrusted data.
pub async fn board_summary(cx: &ToolCx<'_>, a: BoardSummaryArgs) -> ToolResult {
    let p = api::CardListParams {
        state: None,
        lane: a.lane,
        limit: 200,
    };
    let out = api::card_list(&cx.peer.ctx, p).await?;
    let cards = out.json.get("cards").cloned().unwrap_or(Value::Null);
    let me = agora_store::hex(&cx.peer.ctx.identity.public());
    let mut structured = summarize(&cards, &me);
    let mut lines = vec![summary_text(&structured)];
    // An older daemon cannot give the extras: the summary goes out without them.
    let extras = sched_api::board_extras(&cx.peer.ctx).await.ok();
    let extras_json = match &extras {
        Some(e) => {
            lines.extend(extras_lines(e));
            serde_json::to_value(e).unwrap_or(Value::Null)
        }
        None => Value::Null,
    };
    if let Value::Object(map) = &mut structured {
        map.insert("extras".to_owned(), extras_json);
    }
    Ok(framed_output(Out {
        json: structured,
        text: lines.join("\n"),
    }))
}

/// The board extras as text lines, in the order the board summary shows them: one "needs owner"
/// line per card, one "wip" line per lane, then the job and lease counts, then the quiet window
/// and the admission pause when set. Card titles, lane and team names and the reasons are written
/// by other agents, so every line goes through `printable`.
fn extras_lines(extras: &BoardExtras) -> Vec<String> {
    let mut lines: Vec<String> = extras
        .needs_owner
        .iter()
        .map(|n| {
            printable(&format!(
                "needs owner: {} ({}): {}",
                n.card, n.title, n.owner_action
            ))
        })
        .collect();
    lines.extend(extras.wip.iter().map(|w| {
        let limit = w.limit.map_or_else(|| "-".to_owned(), |n| n.to_string());
        printable(&format!("wip: {}/{} {}/{limit}", w.team, w.lane, w.count))
    }));
    lines.push(format!(
        "jobs: {} running, {} waiting, {} queued",
        extras.running_jobs, extras.waiting_jobs, extras.queued_tasks
    ));
    lines.push(format!("leases: {} open", extras.open_leases));
    if let Some(quiet) = &extras.quiet {
        lines.push(printable(&format!("quiet: {}", quiet.text)));
    }
    if let Some(reason) = &extras.paused {
        lines.push(printable(&format!("admission paused: {reason}")));
    }
    lines
}

/// Publish a note via `api::board::note_publish`.
pub async fn note_publish(cx: &ToolCx<'_>, a: NotePublishArgs) -> ToolResult {
    let p = api::NotePublishParams {
        id: a.id,
        title: a.title,
        body: a.body,
        tags: a.tags,
        supersedes: a.supersedes,
    };
    Ok(output(api::note_publish(&cx.peer.ctx, p).await?))
}

/// Search notes via `api::board::note_search` (limit default 20, clamped to 1..=200). Text: one line per note with its title, framed as untrusted data.
pub async fn note_search(cx: &ToolCx<'_>, a: NoteSearchArgs) -> ToolResult {
    let p = api::NoteSearchParams {
        query: a.query,
        limit: clamp_limit(a.limit),
    };
    Ok(framed_output(api::note_search(&cx.peer.ctx, p).await?))
}

/// Lifecycle order used for the human text; states not listed follow in name order.
const STATE_ORDER: [&str; 7] = [
    "backlog", "ready", "claimed", "doing", "review", "blocked", "done",
];

/// Reduce the `cards` array of a card list to the board summary JSON. `cards` is expected to be
/// an array of card objects; anything else counts as no cards. An entry without a string `id` is
/// malformed and is skipped. A missing `state` or `lane` is counted under "unknown".
fn summarize(cards: &Value, me_hex: &str) -> Value {
    let entries: &[Value] = cards.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let mut total: u64 = 0;
    let mut by_state: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_lane: BTreeMap<String, u64> = BTreeMap::new();
    let mut mine = Vec::new();
    let mut blocked = Vec::new();
    for card in entries {
        if card.get("id").and_then(Value::as_str).is_none() {
            continue;
        }
        total += 1;
        let state = card
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let lane = card
            .get("lane")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        *by_state.entry(state.to_owned()).or_insert(0) += 1;
        *by_lane.entry(lane.to_owned()).or_insert(0) += 1;
        if card.get("owner").and_then(Value::as_str) == Some(me_hex) {
            mine.push(card.clone());
        }
        if state == "blocked" {
            blocked.push(card.clone());
        }
    }
    serde_json::json!({
        "total": total,
        "by_state": counts_json(&by_state),
        "by_lane": counts_json(&by_lane),
        "mine": mine,
        "blocked": blocked,
    })
}

fn counts_json(counts: &BTreeMap<String, u64>) -> Value {
    Value::Object(
        counts
            .iter()
            .map(|(k, n)| (k.clone(), Value::from(*n)))
            .collect(),
    )
}

/// One line for humans, e.g. "12 cards: 3 ready, 2 doing, 1 blocked; 2 yours". Card titles and
/// lane names are not part of it.
fn summary_text(summary: &Value) -> String {
    let total = summary.get("total").and_then(Value::as_u64).unwrap_or(0);
    if total == 0 {
        return "0 cards".into();
    }
    let mut states: Vec<(&str, u64)> = summary
        .get("by_state")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.as_str(), v.as_u64()?)))
                .collect()
        })
        .unwrap_or_default();
    states.sort_by_key(|(s, _)| {
        STATE_ORDER
            .iter()
            .position(|o| o == s)
            .unwrap_or(STATE_ORDER.len())
    });
    let states = states
        .iter()
        .map(|(s, n)| format!("{n} {s}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mine = summary
        .get("mine")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    format!("{total} cards: {states}; {mine} yours")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::UNTRUSTED_DATA_NOTICE;
    use serde_json::json;

    const ME: &str = "aa11";

    fn cards() -> Value {
        json!([
            {"id": "c1", "title": "one", "lane": "core", "state": "ready", "owner": null},
            {"id": "c2", "title": "two", "lane": "core", "state": "doing", "owner": ME},
            {"id": "c3", "title": "three", "lane": "web", "state": "blocked", "owner": "bb22"},
            {"id": "c4", "title": "four", "lane": "web", "state": "doing", "owner": ME},
            {"id": "c5", "title": "five", "lane": "core", "state": "blocked", "owner": ME},
        ])
    }

    #[test]
    fn counts_by_state_and_lane() {
        let s = summarize(&cards(), ME);
        assert_eq!(s["total"], 5);
        assert_eq!(s["by_state"], json!({"blocked": 2, "doing": 2, "ready": 1}));
        assert_eq!(s["by_lane"], json!({"core": 3, "web": 2}));
    }

    #[test]
    fn mine_and_blocked_lists_hold_full_cards() {
        let s = summarize(&cards(), ME);
        let mine: Vec<&str> = s["mine"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|c| c["id"].as_str())
            .collect();
        assert_eq!(mine, ["c2", "c4", "c5"]);
        let blocked: Vec<&str> = s["blocked"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|c| c["id"].as_str())
            .collect();
        assert_eq!(blocked, ["c3", "c5"]);
        assert_eq!(s["mine"][0]["title"], "two");
    }

    #[test]
    fn framed_output_adds_the_notice_to_an_object_and_frames_the_text() {
        let out = framed_output(Out {
            json: json!({"cards": []}),
            text: "c1  ready  core  x".into(),
        });
        assert_eq!(out.structured["notice"], UNTRUSTED_DATA_NOTICE);
        assert_eq!(out.structured["cards"], json!([]));
        assert!(out.text.starts_with(UNTRUSTED_DATA_NOTICE), "{}", out.text);
        assert!(
            out.text.ends_with("c1  ready  core  x\n</untrusted-data>"),
            "{}",
            out.text
        );
        let not_an_object = framed_output(Out {
            json: json!([1]),
            text: String::new(),
        });
        assert_eq!(not_an_object.structured, json!([1]));
    }

    #[test]
    fn empty_input_gives_zero_counts() {
        for input in [json!([]), json!(null), json!({"cards": []}), json!("x")] {
            let s = summarize(&input, ME);
            assert_eq!(s["total"], 0, "{input}");
            assert_eq!(s["by_state"], json!({}), "{input}");
            assert_eq!(s["by_lane"], json!({}), "{input}");
            assert_eq!(s["mine"], json!([]), "{input}");
            assert_eq!(s["blocked"], json!([]), "{input}");
        }
    }

    #[test]
    fn malformed_entries_are_ignored() {
        let input = json!([
            "not an object",
            42,
            null,
            {"state": "ready", "lane": "core", "owner": ME},
            {"id": 7, "state": "blocked", "owner": ME},
            {"id": "ok", "owner": ME},
        ]);
        let s = summarize(&input, ME);
        assert_eq!(s["total"], 1);
        assert_eq!(s["by_state"], json!({"unknown": 1}));
        assert_eq!(s["by_lane"], json!({"unknown": 1}));
        assert_eq!(s["mine"].as_array().map(Vec::len), Some(1));
        assert_eq!(s["blocked"], json!([]));
    }

    #[test]
    fn text_counts_states_in_lifecycle_order_without_titles() {
        let s = summarize(&cards(), ME);
        assert_eq!(
            summary_text(&s),
            "5 cards: 1 ready, 2 doing, 2 blocked; 3 yours"
        );
        assert_eq!(summary_text(&summarize(&json!([]), ME)), "0 cards");
    }

    fn extras(value: Value) -> BoardExtras {
        serde_json::from_value(value).expect("board extras")
    }

    #[test]
    fn extras_lines_render_every_section_in_order() {
        let e = extras(json!({
            "needs_owner": [{"card": "c1", "title": "one", "owner_action": "claim it"}],
            "wip": [
                {"team": "core", "lane": "doing", "count": 2, "limit": 3},
                {"team": "web", "lane": "ready", "count": 1},
            ],
            "running_jobs": 1,
            "waiting_jobs": 2,
            "queued_tasks": 3,
            "open_leases": 4,
            "quiet": {"state": "window", "seconds_left": 60.0, "text": "quiet until 18:00"},
            "paused": "disk full",
        }));
        assert_eq!(
            extras_lines(&e),
            [
                "needs owner: c1 (one): claim it",
                "wip: core/doing 2/3",
                "wip: web/ready 1/-",
                "jobs: 1 running, 2 waiting, 3 queued",
                "leases: 4 open",
                "quiet: quiet until 18:00",
                "admission paused: disk full",
            ]
        );
    }

    #[test]
    fn extras_lines_of_an_empty_board_keep_only_the_counts() {
        let e = extras(json!({}));
        assert_eq!(
            extras_lines(&e),
            ["jobs: 0 running, 0 waiting, 0 queued", "leases: 0 open",]
        );
    }

    #[test]
    fn extras_lines_omit_quiet_and_pause_when_unset() {
        let e = extras(json!({"running_jobs": 5, "open_leases": 1}));
        let lines = extras_lines(&e);
        assert!(lines.iter().all(|l| !l.starts_with("quiet:")), "{lines:?}");
        assert!(
            lines.iter().all(|l| !l.starts_with("admission paused:")),
            "{lines:?}"
        );
        assert_eq!(lines.len(), 2, "{lines:?}");
    }

    #[test]
    fn extras_lines_escape_control_characters_in_untrusted_names() {
        let e = extras(json!({
            "needs_owner": [{"card": "c1", "title": "a\nb\u{1b}[2J", "owner_action": "x"}],
            "wip": [{"team": "t\tm", "lane": "l", "count": 0}],
        }));
        let lines = extras_lines(&e);
        assert_eq!(lines[0], "needs owner: c1 (a\\nb\\x1b[2J): x");
        assert_eq!(lines[1], "wip: t\\tm/l 0/-");
        for line in &lines {
            assert!(!line.contains(['\n', '\t', '\u{1b}']), "{line:?}");
        }
    }

    #[test]
    fn extras_lines_list_one_needs_owner_line_per_card() {
        let e = extras(json!({
            "needs_owner": [
                {"card": "c1", "title": "one", "owner_action": "claim it"},
                {"card": "c2", "title": "two", "owner_action": "reclaim it"},
            ],
        }));
        let lines = extras_lines(&e);
        assert_eq!(lines[0], "needs owner: c1 (one): claim it");
        assert_eq!(lines[1], "needs owner: c2 (two): reclaim it");
        assert_eq!(lines.len(), 4, "{lines:?}");
    }
}
