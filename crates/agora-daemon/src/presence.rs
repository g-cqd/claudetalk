//! Who is online: a counted set of identities that hold a subscribed connection.

use agora_store::Pub;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

type Counts = Arc<Mutex<HashMap<Pub, usize>>>;

/// Counted set of online identities (cheap to clone; clones share the set).
/// An identity is online while at least one [`PresenceGuard`] for it is alive.
#[derive(Clone, Default)]
pub struct Presence {
    counts: Counts,
}

/// Keeps one count of an identity online; dropping it removes that count.
#[must_use = "the identity is online only while the guard is alive"]
pub struct PresenceGuard {
    counts: Counts,
    pk: Pub,
}

/// Lock the map; a poisoned lock still holds consistent counts (every critical
/// section is a single map operation), so recover the data.
fn lock(m: &Mutex<HashMap<Pub, usize>>) -> MutexGuard<'_, HashMap<Pub, usize>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Presence {
    /// Mark `pk` online until the returned guard is dropped (count + 1).
    pub fn enter(&self, pk: Pub) -> PresenceGuard {
        let mut counts = lock(&self.counts);
        let n = counts.entry(pk).or_insert(0);
        *n = n.saturating_add(1);
        PresenceGuard {
            counts: Arc::clone(&self.counts),
            pk,
        }
    }

    /// Every online identity, sorted by key bytes (which is hex order).
    pub fn online(&self) -> Vec<Pub> {
        let mut out: Vec<Pub> = lock(&self.counts).keys().copied().collect();
        out.sort_unstable();
        out
    }

    pub fn is_online(&self, pk: &Pub) -> bool {
        lock(&self.counts).contains_key(pk)
    }
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        let mut counts = lock(&self.counts);
        if let Some(n) = counts.get_mut(&self.pk) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.remove(&self.pk);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_drop_decrements_and_removes_at_zero() {
        let p = Presence::default();
        let a = [1u8; 32];
        assert!(!p.is_online(&a));
        let g = p.enter(a);
        assert!(p.is_online(&a));
        drop(g);
        assert!(!p.is_online(&a));
        assert!(p.online().is_empty());
    }

    #[test]
    fn two_guards_for_one_key_keep_it_online_until_both_drop() {
        let p = Presence::default();
        let a = [1u8; 32];
        let g1 = p.enter(a);
        let g2 = p.enter(a);
        drop(g1);
        assert!(p.is_online(&a));
        assert_eq!(p.online(), vec![a]);
        drop(g2);
        assert!(!p.is_online(&a));
    }

    #[test]
    fn online_is_sorted_and_clones_share_state() {
        let p = Presence::default();
        let q = p.clone();
        let _c = p.enter([9u8; 32]);
        let _a = q.enter([1u8; 32]);
        let _b = p.enter([5u8; 32]);
        assert_eq!(q.online(), vec![[1u8; 32], [5u8; 32], [9u8; 32]]);
        assert!(q.is_online(&[9u8; 32]));
    }
}
