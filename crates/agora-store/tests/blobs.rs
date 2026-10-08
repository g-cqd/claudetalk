//! Public-API tests for blob storage: spilling large blobs to files under the blob dir,
//! verifying them on read, reopening the database, and the per-uploader size cap, quota
//! and identity rules enforced by `put_blob_for`.

mod common;
use common::*;

use agora_core::{Role, TransitionError};
use agora_crypto::{Identity, content_hash};
use agora_store::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SCRATCH: AtomicU64 = AtomicU64::new(0);

/// A unique directory under the system temp dir, removed when the test ends.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let n = SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("agora-blobs-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self(path)
    }

    fn db(&self) -> PathBuf {
        self.0.join("db.sqlite")
    }

    fn blobs(&self) -> PathBuf {
        self.0.join("blobs")
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Where a spilled blob must live: `<blob dir>/<hash[0..2]>/<hash>`.
fn spill_path(blob_dir: &Path, hash: &str) -> PathBuf {
    blob_dir.join(&hash[..2]).join(hash)
}

/// A file-backed store whose blobs spill under `dir`, with an owner and a registered
/// worker (alice), so uploads by alice are accepted.
fn file_store(dir: &ScratchDir) -> (Store, Identity) {
    let mut s = Store::open_with_blob_dir(&dir.db(), &dir.blobs()).expect("open store");
    let owner = Identity::from_seed([1; 32]);
    let alice = Identity::from_seed([3; 32]);
    s.bootstrap_owner(NOW, &owner.public()).expect("bootstrap");
    s.register_identity(NOW, &alice.public(), "alice")
        .expect("register alice");
    (s, alice)
}

#[test]
fn blob_over_the_threshold_spills_to_its_hash_prefix_path_and_reads_back() {
    let dir = ScratchDir::new();
    let (mut s, alice) = file_store(&dir);
    let bytes = vec![0xab; SPILL_THRESHOLD + 1];
    let hash = s
        .put_blob_for(NOW, &alice.public(), &bytes, MAX_BLOB, i64::MAX)
        .expect("upload");
    assert_eq!(
        std::fs::read(spill_path(&dir.blobs(), &hash)).expect("spill file"),
        bytes
    );
    assert_eq!(s.get_blob(&hash).expect("get blob"), Some(bytes));
}

#[test]
fn blob_of_exactly_the_threshold_stays_inline_without_a_file() {
    let dir = ScratchDir::new();
    let (mut s, alice) = file_store(&dir);
    let bytes = vec![0xcd; SPILL_THRESHOLD];
    let hash = s
        .put_blob_for(NOW, &alice.public(), &bytes, MAX_BLOB, i64::MAX)
        .expect("upload");
    assert!(!spill_path(&dir.blobs(), &hash).exists());
    assert_eq!(s.get_blob(&hash).expect("get blob"), Some(bytes));
}

#[test]
fn large_blob_without_a_blob_dir_stays_inline_and_round_trips() {
    let mut w = world();
    let bytes = vec![0x5a; SPILL_THRESHOLD + 1];
    let hash =
        w.s.put_blob_for(NOW, &w.alice.public(), &bytes, MAX_BLOB, i64::MAX)
            .expect("upload");
    assert_eq!(w.s.get_blob(&hash).expect("get blob"), Some(bytes));
}

#[test]
fn storing_the_same_spilled_blob_twice_is_idempotent() {
    let dir = ScratchDir::new();
    let (mut s, alice) = file_store(&dir);
    let bytes = vec![0x42; SPILL_THRESHOLD + 1];
    let first = s
        .put_blob_for(NOW, &alice.public(), &bytes, MAX_BLOB, i64::MAX)
        .expect("first upload");
    let second = s
        .put_blob_for(NOW, &alice.public(), &bytes, MAX_BLOB, i64::MAX)
        .expect("second upload");
    assert_eq!(first, second);
    assert_eq!(s.get_blob(&first).expect("get blob"), Some(bytes));
}

#[test]
fn spill_file_altered_by_one_byte_is_refused_as_corrupt() {
    let dir = ScratchDir::new();
    let (mut s, alice) = file_store(&dir);
    let bytes = vec![0x33; SPILL_THRESHOLD + 1];
    let hash = s
        .put_blob_for(NOW, &alice.public(), &bytes, MAX_BLOB, i64::MAX)
        .expect("upload");
    let path = spill_path(&dir.blobs(), &hash);
    let mut on_disk = std::fs::read(&path).expect("spill file");
    on_disk[0] ^= 0x01;
    std::fs::write(&path, &on_disk).expect("alter spill file");
    assert!(matches!(
        s.get_blob(&hash),
        Err(StoreError::Invalid("blob file corrupt"))
    ));
}

#[test]
fn spill_file_deleted_is_reported_as_missing() {
    let dir = ScratchDir::new();
    let (mut s, alice) = file_store(&dir);
    let bytes = vec![0x44; SPILL_THRESHOLD + 1];
    let hash = s
        .put_blob_for(NOW, &alice.public(), &bytes, MAX_BLOB, i64::MAX)
        .expect("upload");
    std::fs::remove_file(spill_path(&dir.blobs(), &hash)).expect("delete spill file");
    assert!(matches!(
        s.get_blob(&hash),
        Err(StoreError::Invalid("blob file missing"))
    ));
}

#[test]
fn spilled_blob_is_still_readable_after_reopening_the_database() {
    let dir = ScratchDir::new();
    let bytes = vec![0x77; SPILL_THRESHOLD + 1];
    let hash = {
        let (mut s, alice) = file_store(&dir);
        s.put_blob_for(NOW, &alice.public(), &bytes, MAX_BLOB, i64::MAX)
            .expect("upload")
    };
    let reopened = Store::open_with_blob_dir(&dir.db(), &dir.blobs()).expect("reopen store");
    assert_eq!(reopened.get_blob(&hash).expect("get blob"), Some(bytes));
}

#[test]
fn upload_within_the_quota_is_stored_and_charged_to_the_uploader() {
    let mut w = world();
    w.s.put_blob_for(NOW, &w.alice.public(), b"0123456789", 100, 30)
        .expect("first upload fits");
    assert_eq!(w.s.blob_usage(&w.alice.public()).expect("usage"), 10);
    // Filling the quota exactly is allowed.
    w.s.put_blob_for(NOW, &w.alice.public(), &[7; 20], 100, 30)
        .expect("upload up to the quota");
    assert_eq!(w.s.blob_usage(&w.alice.public()).expect("usage"), 30);
}

#[test]
fn upload_past_the_quota_is_refused_and_leaves_nothing_behind() {
    let mut w = world();
    w.s.put_blob_for(NOW, &w.alice.public(), &[1; 20], 100, 30)
        .expect("first upload fits");
    let over = [2; 11];
    let err =
        w.s.put_blob_for(NOW, &w.alice.public(), &over, 100, 30)
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("blob quota exceeded")));
    assert_eq!(w.s.blob_usage(&w.alice.public()).expect("usage"), 20);
    assert_eq!(w.s.get_blob(&content_hash(&over)).expect("get blob"), None);
}

#[test]
fn reuploading_a_blob_the_same_uploader_holds_charges_nothing() {
    let mut w = world();
    let bytes = [9; 20];
    // The quota equals the blob's size, so a second charge would be refused.
    let first =
        w.s.put_blob_for(NOW, &w.alice.public(), &bytes, 100, 20)
            .expect("first upload");
    let again =
        w.s.put_blob_for(NOW, &w.alice.public(), &bytes, 100, 20)
            .expect("re-upload is not charged");
    assert_eq!(first, again);
    assert_eq!(w.s.blob_usage(&w.alice.public()).expect("usage"), 20);
}

#[test]
fn another_uploader_of_an_existing_blob_is_charged_for_it() {
    let mut w = world();
    let bytes = [5; 20];
    w.s.put_blob_for(NOW, &w.alice.public(), &bytes, 100, 100)
        .expect("alice uploads");
    w.s.put_blob_for(NOW, &w.bob.public(), &bytes, 100, 100)
        .expect("bob uploads the same bytes");
    assert_eq!(w.s.blob_usage(&w.alice.public()).expect("alice usage"), 20);
    assert_eq!(w.s.blob_usage(&w.bob.public()).expect("bob usage"), 20);
}

#[test]
fn blob_over_the_callers_cap_is_refused_and_not_charged() {
    let mut w = world();
    w.s.put_blob_for(NOW, &w.alice.public(), &[3; 10], 10, 100)
        .expect("exactly the cap is accepted");
    let err =
        w.s.put_blob_for(NOW, &w.alice.public(), &[3; 11], 10, 100)
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("blob too large")));
    assert_eq!(w.s.blob_usage(&w.alice.public()).expect("usage"), 10);
}

#[test]
fn blob_over_max_blob_is_refused_even_when_the_caller_allows_more() {
    let mut w = world();
    let too_big = vec![0u8; MAX_BLOB + 1];
    let err =
        w.s.put_blob_for(NOW, &w.alice.public(), &too_big, usize::MAX, i64::MAX)
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("blob too large")));
    assert_eq!(w.s.blob_usage(&w.alice.public()).expect("usage"), 0);
}

#[test]
fn unregistered_uploader_is_refused() {
    let mut w = world();
    let err =
        w.s.put_blob_for(NOW, &w.eve.public(), b"orphan", 100, 100)
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("unknown identity")));
    assert_eq!(w.s.blob_usage(&w.eve.public()).expect("usage"), 0);
    assert_eq!(
        w.s.get_blob(&content_hash(b"orphan")).expect("get blob"),
        None
    );
}

#[test]
fn revoked_uploader_is_refused() {
    let mut w = world();
    w.s.revoke_identity(NOW, &op(&w.owner, "identity.revoke", &pk_hex(&w.alice), ""))
        .expect("revoke alice");
    let err =
        w.s.put_blob_for(NOW, &w.alice.public(), b"late", 100, 100)
            .unwrap_err();
    assert!(matches!(err, StoreError::Invalid("identity revoked")));
    assert_eq!(
        w.s.get_blob(&content_hash(b"late")).expect("get blob"),
        None
    );
}

#[test]
fn observer_uploader_is_refused_as_forbidden() {
    let mut w = world();
    let err =
        w.s.put_blob_for(NOW, &w.olive.public(), b"look", 100, 100)
            .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden(Role::Observer))
    ));
    assert_eq!(w.s.blob_usage(&w.olive.public()).expect("usage"), 0);
}
