//! Board handlers through the daemon: the card lifecycle and its review gate, card
//! listing and authorization, sprints, and knowledge notes (SPEC "Handlers: board").

mod common;

use agora_daemon::now_ms;
use agora_proto::{
    CardView, ErrorBody, ErrorCode, NoteHit, NoteWire, Reply, Req, SprintSummaryView,
};
use agora_store::SignedNote;
use common::*;

const DAY_MS: i64 = 86_400_000;

type Call = Result<Reply, ErrorBody>;

/// The world, with carol granted the supervisor role by the owner. Carol plans the
/// board; alice and bob are workers.
async fn board_world(d: &TestDaemon) -> World {
    let mut w = world(d).await;
    let op = w.owner.op("role.grant", &w.carol.pk_hex(), "supervisor");
    match w.owner.ok(Req::RoleGrant { op }).await {
        Reply::Unit => {}
        other => panic!("RoleGrant: expected Unit, got {other:?}"),
    }
    w
}

async fn create_card(who: &mut TestClient, card: &str, lane: &str) -> Call {
    let arg = serde_json::json!({"title": "T", "lane": lane}).to_string();
    let op = who.op("card.create", card, &arg);
    who.call(Req::CardCreate { op }).await
}

/// A card.move; the arg is the target state.
async fn move_card(who: &mut TestClient, card: &str, to: &str) -> Call {
    let op = who.op("card.move", card, to);
    who.call(Req::CardMove { op }).await
}

async fn add_evidence(who: &mut TestClient, card: &str, reference: &str) -> Call {
    let op = who.op("card.evidence", card, reference);
    who.call(Req::CardEvidence { op }).await
}

async fn approve_card(who: &mut TestClient, card: &str) -> Call {
    let op = who.op("card.approve", card, "");
    who.call(Req::CardApprove { op }).await
}

async fn reclaim_card(who: &mut TestClient, card: &str) -> Call {
    let op = who.op("card.reclaim", card, "");
    who.call(Req::CardReclaim { op }).await
}

async fn list_cards(
    who: &mut TestClient,
    state: Option<&str>,
    lane: Option<&str>,
) -> Vec<CardView> {
    let req = Req::CardList {
        state: state.map(str::to_string),
        lane: lane.map(str::to_string),
        limit: 100,
    };
    match who.ok(req).await {
        Reply::Cards { cards } => cards,
        other => panic!("CardList: expected Cards, got {other:?}"),
    }
}

/// Sorted ids of the cards matching the filters.
async fn card_ids(who: &mut TestClient, state: Option<&str>, lane: Option<&str>) -> Vec<String> {
    let mut ids: Vec<String> = list_cards(who, state, lane)
        .await
        .into_iter()
        .map(|c| c.id)
        .collect();
    ids.sort();
    ids
}

/// Sorted (id, state) of every card the reader can see.
async fn card_states(who: &mut TestClient) -> Vec<(String, String)> {
    let mut states: Vec<(String, String)> = list_cards(who, None, None)
        .await
        .into_iter()
        .map(|c| (c.id, c.state))
        .collect();
    states.sort();
    states
}

fn pair(id: &str, state: &str) -> (String, String) {
    (id.to_string(), state.to_string())
}

/// Carol creates c1 in lane "l" and promotes it to ready; alice claims it, starts it,
/// attaches evidence and sends it to review. Every step must succeed.
async fn drive_c1_to_review(w: &mut World) {
    create_card(&mut w.carol, "c1", "l")
        .await
        .expect("create c1");
    move_card(&mut w.carol, "c1", "ready")
        .await
        .expect("backlog -> ready as supervisor");
    move_card(&mut w.alice, "c1", "claimed")
        .await
        .expect("ready -> claimed");
    move_card(&mut w.alice, "c1", "doing")
        .await
        .expect("claimed -> doing");
    add_evidence(&mut w.alice, "c1", "log://x")
        .await
        .expect("attach evidence");
    move_card(&mut w.alice, "c1", "review")
        .await
        .expect("doing -> review");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn done_is_refused_in_review_until_an_independent_approval_exists() {
    let d = start().await;
    let mut w = board_world(&d).await;
    drive_c1_to_review(&mut w).await;

    let err = move_card(&mut w.alice, "c1", "done")
        .await
        .expect_err("done without approval must be refused");
    assert_eq!(err.code, ErrorCode::Invalid);
    assert_eq!(card_states(&mut w.owner).await, vec![pair("c1", "review")]);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approval_by_another_worker_lets_the_claim_owner_finish_a_reviewed_card() {
    let d = start().await;
    let mut w = board_world(&d).await;
    drive_c1_to_review(&mut w).await;

    approve_card(&mut w.bob, "c1")
        .await
        .expect("bob approves in review");
    move_card(&mut w.alice, "c1", "done")
        .await
        .expect("done after an independent approval");
    assert_eq!(card_states(&mut w.owner).await, vec![pair("c1", "done")]);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_claim_owners_own_approval_does_not_satisfy_the_review_gate() {
    let d = start().await;
    let mut w = board_world(&d).await;
    drive_c1_to_review(&mut w).await;

    approve_card(&mut w.alice, "c1")
        .await
        .expect("alice approves her own card in review");
    let err = move_card(&mut w.alice, "c1", "done")
        .await
        .expect_err("done on the claim owner's approval alone must be refused");
    assert_eq!(err.code, ErrorCode::Invalid);
    assert_eq!(card_states(&mut w.owner).await, vec![pair("c1", "review")]);

    approve_card(&mut w.bob, "c1")
        .await
        .expect("bob approves in review");
    move_card(&mut w.alice, "c1", "done")
        .await
        .expect("done after an independent approval");
    assert_eq!(card_states(&mut w.owner).await, vec![pair("c1", "done")]);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_review_gate_done_needs_evidence_but_no_approval() {
    let d = start_with(|c| c.require_review = false).await;
    let mut w = board_world(&d).await;
    create_card(&mut w.carol, "c1", "l")
        .await
        .expect("create c1");
    move_card(&mut w.carol, "c1", "ready")
        .await
        .expect("backlog -> ready");
    move_card(&mut w.alice, "c1", "claimed")
        .await
        .expect("claim");
    move_card(&mut w.alice, "c1", "doing").await.expect("start");

    let err = move_card(&mut w.alice, "c1", "done")
        .await
        .expect_err("done without evidence must be refused");
    assert_eq!(err.code, ErrorCode::Invalid);
    assert_eq!(card_states(&mut w.owner).await, vec![pair("c1", "doing")]);

    add_evidence(&mut w.alice, "c1", "log://x")
        .await
        .expect("attach evidence");
    move_card(&mut w.alice, "c1", "done")
        .await
        .expect("done with evidence and no approval");
    assert_eq!(card_states(&mut w.owner).await, vec![pair("c1", "done")]);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_op_signed_by_another_identity_is_forbidden_on_this_connection() {
    let d = start().await;
    let mut w = board_world(&d).await;
    let arg = serde_json::json!({"title": "T", "lane": "l"}).to_string();
    let op = w.bob.op("card.create", "c1", &arg);

    let err = w
        .alice
        .call(Req::CardCreate { op })
        .await
        .expect_err("bob's op on alice's connection must be refused");
    assert_eq!(err.code, ErrorCode::Forbidden);
    assert!(card_states(&mut w.owner).await.is_empty());

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn card_list_returns_only_the_cards_in_the_requested_state() {
    let d = start().await;
    let mut w = board_world(&d).await;
    for card in ["a", "b", "c"] {
        create_card(&mut w.carol, card, "l")
            .await
            .expect("create card");
    }
    move_card(&mut w.carol, "a", "ready")
        .await
        .expect("a -> ready");

    assert_eq!(card_ids(&mut w.alice, Some("ready"), None).await, ["a"]);
    assert_eq!(
        card_ids(&mut w.alice, Some("backlog"), None).await,
        ["b", "c"]
    );

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn card_list_returns_only_the_cards_in_the_requested_lane() {
    let d = start().await;
    let mut w = board_world(&d).await;
    create_card(&mut w.carol, "a", "x").await.expect("create a");
    create_card(&mut w.carol, "b", "x").await.expect("create b");
    create_card(&mut w.carol, "c", "y").await.expect("create c");

    assert_eq!(card_ids(&mut w.alice, None, Some("y")).await, ["c"]);
    assert_eq!(card_ids(&mut w.alice, None, Some("x")).await, ["a", "b"]);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn card_list_with_an_unknown_state_is_invalid() {
    let d = start().await;
    let mut w = board_world(&d).await;

    let err = w
        .alice
        .call(Req::CardList {
            state: Some("finished".to_string()),
            lane: None,
            limit: 10,
        })
        .await
        .expect_err("unknown state must be refused");
    assert_eq!(err.code, ErrorCode::Invalid);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn card_list_from_an_unregistered_key_is_forbidden() {
    let d = start().await;
    let _ = world(&d).await;
    let mut stranger = connect(&d, ident(9)).await;

    let err = stranger
        .call(Req::CardList {
            state: None,
            lane: None,
            limit: 10,
        })
        .await
        .expect_err("an unregistered key must not read the board");
    assert_eq!(err.code, ErrorCode::Forbidden);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supervisor_reclaim_returns_a_claimed_card_to_ready_without_an_owner() {
    let d = start().await;
    let mut w = board_world(&d).await;
    create_card(&mut w.carol, "c1", "l")
        .await
        .expect("create c1");
    move_card(&mut w.carol, "c1", "ready")
        .await
        .expect("backlog -> ready");
    move_card(&mut w.alice, "c1", "claimed")
        .await
        .expect("claim");

    let reply = reclaim_card(&mut w.carol, "c1")
        .await
        .expect("supervisor reclaims a claimed card");
    assert_eq!(reply, Reply::Unit);
    let cards = list_cards(&mut w.owner, None, None).await;
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].state, "ready");
    assert_eq!(cards[0].owner, None);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_cannot_promote_a_backlog_card() {
    let d = start().await;
    let mut w = board_world(&d).await;
    create_card(&mut w.carol, "c1", "l")
        .await
        .expect("create c1");

    let err = move_card(&mut w.alice, "c1", "ready")
        .await
        .expect_err("a worker's backlog -> ready must be refused");
    assert_eq!(err.code, ErrorCode::Forbidden);
    assert_eq!(card_states(&mut w.owner).await, vec![pair("c1", "backlog")]);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reclaim_of_a_card_that_is_not_claimed_is_invalid() {
    let d = start().await;
    let mut w = board_world(&d).await;
    create_card(&mut w.carol, "c1", "l")
        .await
        .expect("create c1");
    move_card(&mut w.carol, "c1", "ready")
        .await
        .expect("backlog -> ready");

    let err = reclaim_card(&mut w.carol, "c1")
        .await
        .expect_err("reclaim of a ready card must be refused");
    assert_eq!(err.code, ErrorCode::Invalid);
    assert_eq!(card_states(&mut w.owner).await, vec![pair("c1", "ready")]);

    d.stop().await;
}

// ---------------------------------------------------------------- sprints

fn sprint_arg() -> String {
    let now = now_ms();
    serde_json::json!({"name": "S1", "goal": "g", "starts": now, "ends": now + DAY_MS}).to_string()
}

async fn sprint_create(who: &mut TestClient, sprint: &str, arg: &str) -> Call {
    let op = who.op("sprint.create", sprint, arg);
    who.call(Req::SprintCreate { op }).await
}

async fn sprint_add(who: &mut TestClient, sprint: &str, card: &str) -> Call {
    let op = who.op("sprint.add", sprint, card);
    who.call(Req::SprintAdd { op }).await
}

async fn sprint_start(who: &mut TestClient, sprint: &str) -> Call {
    let op = who.op("sprint.start", sprint, "");
    who.call(Req::SprintStart { op }).await
}

async fn sprint_close(who: &mut TestClient, sprint: &str) -> Call {
    let op = who.op("sprint.close", sprint, "");
    who.call(Req::SprintClose { op }).await
}

async fn sprint_summary(who: &mut TestClient, sprint: &str) -> SprintSummaryView {
    match who
        .ok(Req::SprintSummary {
            sprint: sprint.to_string(),
        })
        .await
    {
        Reply::Sprint { summary } => summary,
        other => panic!("SprintSummary: expected Sprint, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sprint_summary_counts_done_in_flight_and_blocked_cards_of_this_sprint_only() {
    let d = start().await;
    let mut w = board_world(&d).await;
    // In s1: d (done), k (claimed), o (doing), r (review), b (blocked), y (ready).
    // x (ready) sits in another sprint, s2, and must not be counted for s1.
    for card in ["d", "k", "o", "r", "b", "y", "x"] {
        create_card(&mut w.carol, card, "l")
            .await
            .expect("create card");
        move_card(&mut w.carol, card, "ready")
            .await
            .expect("-> ready");
    }
    sprint_create(&mut w.carol, "s1", &sprint_arg())
        .await
        .expect("supervisor creates a sprint");
    for card in ["d", "k", "o", "r", "b", "y"] {
        sprint_add(&mut w.carol, "s1", card)
            .await
            .expect("add to sprint");
    }
    sprint_create(&mut w.carol, "s2", &sprint_arg())
        .await
        .expect("supervisor creates a second, planned sprint");
    sprint_add(&mut w.carol, "s2", "x")
        .await
        .expect("add x to s2");
    sprint_start(&mut w.carol, "s1")
        .await
        .expect("start sprint");

    for card in ["k", "o", "r", "d"] {
        move_card(&mut w.alice, card, "claimed")
            .await
            .expect("claim");
    }
    for card in ["o", "r", "d"] {
        move_card(&mut w.alice, card, "doing")
            .await
            .expect("start work");
    }
    for card in ["r", "d"] {
        move_card(&mut w.alice, card, "review")
            .await
            .expect("to review");
    }
    add_evidence(&mut w.alice, "d", "log://d")
        .await
        .expect("evidence");
    approve_card(&mut w.bob, "d").await.expect("bob approves d");
    move_card(&mut w.alice, "d", "done")
        .await
        .expect("d -> done");
    move_card(&mut w.carol, "b", "blocked")
        .await
        .expect("ready -> blocked");

    let summary = sprint_summary(&mut w.alice, "s1").await;
    assert_eq!(
        summary,
        SprintSummaryView {
            total: 6,
            done: 1,
            in_flight: 3,
            blocked: 1,
        }
    );

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closed_sprint_refuses_new_cards() {
    let d = start().await;
    let mut w = board_world(&d).await;
    create_card(&mut w.carol, "c1", "l")
        .await
        .expect("create c1");
    create_card(&mut w.carol, "c2", "l")
        .await
        .expect("create c2");
    sprint_create(&mut w.carol, "s1", &sprint_arg())
        .await
        .expect("create sprint");
    sprint_add(&mut w.carol, "s1", "c1").await.expect("add c1");
    sprint_start(&mut w.carol, "s1").await.expect("start");
    let reply = sprint_close(&mut w.carol, "s1")
        .await
        .expect("close the active sprint");
    assert_eq!(reply, Reply::Unit);

    let err = sprint_add(&mut w.carol, "s1", "c2")
        .await
        .expect_err("adding to a closed sprint must be refused");
    assert_eq!(err.code, ErrorCode::Invalid);

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_cannot_create_a_sprint() {
    let d = start().await;
    let mut w = board_world(&d).await;

    let err = sprint_create(&mut w.alice, "s1", &sprint_arg())
        .await
        .expect_err("worker SprintCreate must be refused");
    assert_eq!(err.code, ErrorCode::Forbidden);

    d.stop().await;
}

// ---------------------------------------------------------------- notes

/// A note signed by `author`, with the author key set by `sign`.
fn note_wire(
    author: &TestClient,
    id: &str,
    title: &str,
    body: &str,
    supersedes: Option<&str>,
) -> NoteWire {
    let signed = SignedNote {
        id: id.to_string(),
        author: [0; 32],
        ts: now_ms(),
        title: title.to_string(),
        body: body.to_string(),
        tags: String::new(),
        supersedes: supersedes.map(str::to_string),
        sig: vec![],
    }
    .sign(&author.id);
    NoteWire::from_note(&signed)
}

async fn publish_note(who: &mut TestClient, note: NoteWire) -> Call {
    who.call(Req::NotePublish { note }).await
}

async fn search_notes(who: &mut TestClient, query: &str) -> Vec<NoteHit> {
    match who
        .ok(Req::NoteSearch {
            query: query.to_string(),
            limit: 10,
        })
        .await
    {
        Reply::Notes { notes } => notes,
        other => panic!("NoteSearch: expected Notes, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_published_note_is_found_by_a_word_in_its_body() {
    let d = start().await;
    let mut w = world(&d).await;
    let note = note_wire(
        &w.alice,
        "n1",
        "Retry policy",
        "exponential backoff with jitter",
        None,
    );
    publish_note(&mut w.alice, note)
        .await
        .expect("publish note");

    assert_eq!(
        search_notes(&mut w.bob, "jitter").await,
        vec![NoteHit {
            id: "n1".to_string(),
            title: "Retry policy".to_string(),
        }]
    );

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_superseded_by_its_author_is_hidden_from_search() {
    let d = start().await;
    let mut w = world(&d).await;
    let old = note_wire(&w.alice, "n1", "Old", "zebra crossing rules", None);
    publish_note(&mut w.alice, old).await.expect("publish n1");
    let new = note_wire(
        &w.alice,
        "n2",
        "New",
        "zebra crossing rules, revised",
        Some("n1"),
    );
    publish_note(&mut w.alice, new).await.expect("supersede n1");

    assert_eq!(
        search_notes(&mut w.bob, "zebra").await,
        vec![NoteHit {
            id: "n2".to_string(),
            title: "New".to_string(),
        }]
    );

    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_note_signed_by_another_key_is_forbidden_on_this_connection() {
    let d = start().await;
    let mut w = world(&d).await;
    let note = note_wire(&w.bob, "n1", "Forged", "forged jitter", None);

    let err = publish_note(&mut w.alice, note)
        .await
        .expect_err("bob's note on alice's connection must be refused");
    assert_eq!(err.code, ErrorCode::Forbidden);
    assert!(search_notes(&mut w.bob, "forged").await.is_empty());

    d.stop().await;
}
