//! Fixtures shared by the card-extension unit tests: a store with a bootstrapped owner,
//! two supervisors and workers, and a registered system identity.

use super::*;
use crate::hex;
use agora_core::TransitionError;
use agora_crypto::Identity;
use rusqlite::params;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) const NOW: i64 = 1_700_000_000_000;
static NONCES: AtomicU64 = AtomicU64::new(0);

pub(super) fn sign(id: &Identity, kind: &str, subject: &str, arg: &str, now: i64) -> SignedOp {
    let n = NONCES.fetch_add(1, Ordering::Relaxed);
    SignedOp::new(kind, subject, arg, now, &format!("n{n}")).sign(id)
}

pub(super) struct Cast {
    pub(super) s: Store,
    pub(super) owner: Identity,
    pub(super) sup: Identity,
    pub(super) alice: Identity,
    pub(super) bob: Identity,
    pub(super) sys: Identity,
}

/// Owner bootstrapped; sup is a supervisor; alice and bob are workers; sys is the system key.
pub(super) fn cast() -> Cast {
    let mut s = Store::open_in_memory().unwrap();
    let owner = Identity::from_seed([1; 32]);
    let sup = Identity::from_seed([2; 32]);
    let alice = Identity::from_seed([3; 32]);
    let bob = Identity::from_seed([4; 32]);
    let sys = Identity::from_seed([9; 32]);
    s.bootstrap_owner(NOW, &owner.public()).unwrap();
    for (id, label) in [(&sup, "sup"), (&alice, "alice"), (&bob, "bob")] {
        s.register_identity(NOW, &id.public(), label).unwrap();
    }
    let grant = sign(&owner, "role.grant", &hex(&sup.public()), "supervisor", NOW);
    s.grant_role(NOW, &grant).unwrap();
    s.register_system_identity(NOW, &sys.public()).unwrap();
    Cast {
        s,
        owner,
        sup,
        alice,
        bob,
        sys,
    }
}

/// Creates a backlog card `id` in `team` (title = id) as the supervisor.
pub(super) fn make_in(c: &mut Cast, id: &str, team: &str) {
    let arg = serde_json::json!({"title": id, "team": team}).to_string();
    let op = sign(&c.sup, "card.create", id, &arg, NOW);
    c.s.create_card(NOW, &op).unwrap();
}

pub(super) fn make(c: &mut Cast, id: &str) {
    make_in(c, id, "");
}

pub(super) fn mv(s: &mut Store, who: &Identity, id: &str, to: &str) -> Result<(), StoreError> {
    s.transition(NOW, &sign(who, "card.move", id, to, NOW), false)
}

/// Card `id` ready, then claimed by alice (so alice owns it).
pub(super) fn claimed_by_alice(c: &mut Cast, id: &str) {
    make(c, id);
    mv(&mut c.s, &c.sup, id, "ready").unwrap();
    mv(&mut c.s, &c.alice, id, "claimed").unwrap();
}

pub(super) fn ext(c: &Cast, id: &str) -> CardExt {
    c.s.card_ext(&[id.to_owned()]).unwrap().remove(0)
}

pub(super) fn state_of(c: &Cast, id: &str) -> String {
    c.s.conn
        .query_row("SELECT state FROM cards WHERE id=?", [id], |r| r.get(0))
        .unwrap()
}

pub(super) fn add_task(c: &Cast, id: &str, card: Option<&str>, state: &str) {
    c.s.conn
        .execute(
            "INSERT INTO work_tasks(id, author, queued, state, card) VALUES (?,?,?,?,?)",
            params![id, "x", 0, state, card],
        )
        .unwrap();
}

pub(super) fn notices_for(c: &Cast, subject: &str) -> Vec<(String, String, String)> {
    let mut st =
        c.s.conn
            .prepare("SELECT target, kind, text FROM notices WHERE subject=? ORDER BY seq")
            .unwrap();
    st.query_map([subject], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

pub(super) fn is_forbidden(err: &StoreError, role: Role) -> bool {
    matches!(err, StoreError::Transition(TransitionError::Forbidden(r)) if *r == role)
}
