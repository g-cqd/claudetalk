#![allow(dead_code)]
//! Shared fixtures for the public-API tests. Each test file pulls this in with
//! `mod common; use common::*;`.

use agora_crypto::Identity;
use agora_store::{SignedOp, Store, hex};
use std::sync::atomic::{AtomicU64, Ordering};

/// Fixed daemon clock in milliseconds. Operations are signed at this time
/// unless a test says otherwise.
pub const NOW: i64 = 1_700_000_000_000;

static NONCES: AtomicU64 = AtomicU64::new(0);

/// A nonce that no call in this process will produce twice.
pub fn nonce() -> String {
    format!("n{}", NONCES.fetch_add(1, Ordering::Relaxed))
}

fn signed(id: &Identity, kind: &str, subject: &str, arg: &str, ts: i64, nonce: &str) -> SignedOp {
    SignedOp::new(kind, subject, arg, ts, nonce).sign(id)
}

/// Operation signed at `NOW` with a fresh nonce.
pub fn op(id: &Identity, kind: &str, subject: &str, arg: &str) -> SignedOp {
    signed(id, kind, subject, arg, NOW, &nonce())
}

/// Operation signed at `NOW` with an explicit nonce, for replay tests.
pub fn op_n(id: &Identity, kind: &str, subject: &str, arg: &str, nonce: &str) -> SignedOp {
    signed(id, kind, subject, arg, NOW, nonce)
}

/// Operation signed at an explicit timestamp, for clock tests.
pub fn op_at(id: &Identity, kind: &str, subject: &str, arg: &str, ts: i64) -> SignedOp {
    signed(id, kind, subject, arg, ts, &nonce())
}

/// Hex form of an identity's public key, as used in subjects and args.
pub fn pk_hex(id: &Identity) -> String {
    hex(&id.public())
}

pub struct World {
    pub s: Store,
    pub owner: Identity,
    pub sup: Identity,
    pub alice: Identity,
    pub bob: Identity,
    pub carol: Identity,
    pub olive: Identity,
    /// Generated but never registered: an unknown identity.
    pub eve: Identity,
}

/// Owner bootstrapped; sup is a supervisor and olive an observer (both granted by
/// the owner through real role.grant ops); alice, bob and carol are workers.
/// eve is never registered.
pub fn world() -> World {
    let mut s = Store::open_in_memory().unwrap();
    let owner = Identity::from_seed([1; 32]);
    let sup = Identity::from_seed([2; 32]);
    let alice = Identity::from_seed([3; 32]);
    let bob = Identity::from_seed([4; 32]);
    let carol = Identity::from_seed([7; 32]);
    let olive = Identity::from_seed([8; 32]);
    let eve = Identity::from_seed([6; 32]);
    s.bootstrap_owner(NOW, &owner.public()).unwrap();
    for (id, label) in [
        (&sup, "sup"),
        (&alice, "alice"),
        (&bob, "bob"),
        (&carol, "carol"),
        (&olive, "olive"),
    ] {
        s.register_identity(NOW, &id.public(), label).unwrap();
    }
    s.grant_role(NOW, &op(&owner, "role.grant", &pk_hex(&sup), "supervisor"))
        .unwrap();
    s.grant_role(NOW, &op(&owner, "role.grant", &pk_hex(&olive), "observer"))
        .unwrap();
    World {
        s,
        owner,
        sup,
        alice,
        bob,
        carol,
        olive,
        eve,
    }
}

/// Create a backlog card as the supervisor.
pub fn make_card(w: &mut World, id: &str) {
    w.s.create_card(
        NOW,
        &op(&w.sup, "card.create", id, r#"{"title":"Port scheduler"}"#),
    )
    .unwrap();
}
