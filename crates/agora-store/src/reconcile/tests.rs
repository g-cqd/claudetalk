//! Unit tests for the reconciler store: action application, the input snapshot and the watchdog report.

mod apply;
mod blockers;
mod claims;
mod claims_notice;
mod snapshot;

use super::*;
use agora_crypto::Identity;
use rusqlite::params;

const NOW: i64 = 1_700_000_000_000;
const SEC: i64 = 1_000;
const MIN: i64 = 60 * SEC;

fn world() -> (Store, Identity) {
    let mut s = Store::open_in_memory().expect("store");
    let sys = Identity::from_seed([9; 32]);
    s.register_system_identity(NOW, &sys.public())
        .expect("system identity");
    (s, sys)
}

fn person(seed: u8) -> Identity {
    Identity::from_seed([seed; 32])
}

fn pk_hex(id: &Identity) -> String {
    hex(&id.public())
}

/// Registers `id` with `role`, optionally revoked.
fn enroll(s: &mut Store, id: &Identity, role: &str, revoked: bool) {
    s.register_identity(NOW, &id.public(), "")
        .expect("register");
    s.conn
        .execute(
            "UPDATE identities SET role = ?1, revoked = ?2 WHERE pubkey = ?3",
            params![role, revoked, id.public().as_slice()],
        )
        .expect("role");
}

fn card(
    s: &Store,
    id: &str,
    team: &str,
    lane: &str,
    state: &str,
    owner: Option<&str>,
    updated: i64,
) {
    s.conn
        .execute(
            "INSERT INTO cards(id, title, team, lane, state, owner, created, updated) \
             VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, team, lane, state, owner, updated],
        )
        .expect("card row");
}

fn lease(
    s: &Store,
    id: &str,
    kind: &str,
    holder: &str,
    expires: Option<i64>,
    released: Option<i64>,
) {
    s.conn
        .execute(
            "INSERT INTO leases(id, kind, resource, amount, holder, created, expires, heartbeat, released) \
             VALUES (?1, ?2, '/res', 0, ?3, ?4, ?5, ?4, ?6)",
            params![id, kind, holder, NOW - MIN, expires, released],
        )
        .expect("lease row");
}

fn policy(s: &Store, team: &str, lane: &str, json: &str) {
    s.conn
        .execute(
            "INSERT INTO policies(team, lane, json, updated, by) VALUES (?1, ?2, ?3, ?4, 'x')",
            params![team, lane, json, NOW],
        )
        .expect("policy row");
}

/// A signed reconcile op for `action`, signed by `id` (the system key in the normal path).
fn signed_action(id: &Identity, action: &ReconcileAction, nonce: &str) -> SignedOp {
    let arg = serde_json::to_string(action).expect("action json");
    SignedOp::new(action.op_kind(), action.key(), &arg, NOW, nonce).sign(id)
}

fn apply(
    s: &mut Store,
    sys: &Identity,
    action: &ReconcileAction,
    nonce: &str,
) -> Result<bool, StoreError> {
    s.apply_reconcile(NOW, &signed_action(sys, action, nonce))
}

fn released_of(s: &Store, lease_id: &str) -> Option<i64> {
    s.conn
        .query_row(
            "SELECT released FROM leases WHERE id = ?1",
            [lease_id],
            |r| r.get(0),
        )
        .expect("lease")
}

fn count(s: &Store, sql: &str) -> i64 {
    s.conn.query_row(sql, [], |r| r.get(0)).expect("count")
}
