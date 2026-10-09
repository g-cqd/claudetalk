use super::*;

/// Acquires a path lease for alice with a 60 s ttl at the start instant; returns the lease as created.
async fn path_lease(a: &mut TestClient) -> LeaseRecord {
    let arg = json!({"kind": "path", "resource": "/agora-test/reconciler-lease", "ttl_s": 60});
    let op = a.op("lease.acquire", "", &arg.to_string());
    match a.ok(Req::LeaseAcquire { op }).await {
        Reply::Lease { lease } => lease,
        other => panic!("lease.acquire: expected Lease, got {other:?}"),
    }
}

async fn lease_list(c: &mut TestClient, active_only: bool) -> Vec<LeaseRecord> {
    match c
        .ok(Req::LeaseList {
            kind: None,
            holder: None,
            active_only,
        })
        .await
    {
        Reply::Leases { leases } => leases,
        other => panic!("lease.list: expected Leases, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_path_lease_is_released_once_its_ttl_has_passed() {
    let mut r = rig().await;
    let lease = path_lease(&mut r.w.alice).await;
    assert_eq!(lease.expires_ms, Some(r.start + 60 * SECOND));

    r.clock.pass_at(r.start + 61 * SECOND).await;

    assert!(
        lease_list(&mut r.w.alice, true).await.is_empty(),
        "an expired lease is no longer active"
    );
    let all = lease_list(&mut r.w.alice, false).await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, lease.id);
    assert_eq!(
        all[0].released_ms,
        Some(r.start + 61 * SECOND),
        "the reconciler releases it at the pass's time"
    );
    r.d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_path_lease_is_held_until_its_deadline_and_released_at_it() {
    let mut r = rig().await;
    let lease = path_lease(&mut r.w.alice).await;

    // The reconciler sleeps one interval after each pass, so its passes fall at 30 s and 60 s.
    r.clock.pass_at(r.start + 30 * SECOND).await;
    let active = lease_list(&mut r.w.alice, true).await;
    assert_eq!(
        active.len(),
        1,
        "a pass before the deadline releases nothing"
    );
    assert_eq!(active[0].id, lease.id);
    assert_eq!(active[0].released_ms, None);

    // The deadline itself is expired: the rule is `expires_ms <= now`.
    r.clock.pass_at(r.start + 60 * SECOND).await;
    assert!(lease_list(&mut r.w.alice, true).await.is_empty());
    let all = lease_list(&mut r.w.alice, false).await;
    assert_eq!(all[0].released_ms, Some(r.start + 60 * SECOND));
    r.d.stop().await;
}
