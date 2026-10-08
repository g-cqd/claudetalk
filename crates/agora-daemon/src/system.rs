//! The daemon's own signing key (`<home>/system.seed`). The store registers it as the system
//! identity (role observer) that may sign only `agora_store::SYSTEM_KINDS`.

use agora_crypto::Identity;
use agora_store::SignedOp;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// File name of the system seed under the daemon home.
pub const SEED_FILE: &str = "system.seed";

/// `<home>/system.seed`: 32 random bytes, 0600, opened with O_NOFOLLOW, a regular file owned by the daemon
/// user and not accessible to group or others; created (atomically, via a temp file) if absent.
pub fn load_or_create(home: &Path) -> std::io::Result<Identity> {
    let path = home.join(SEED_FILE);
    match read_seed(&path) {
        Ok(seed) => return Ok(Identity::from_seed(seed)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let fresh = agora_crypto::random_bytes::<32>().map_err(std::io::Error::other)?;
    let nonce =
        u64::from_le_bytes(agora_crypto::random_bytes::<8>().map_err(std::io::Error::other)?);
    let tmp = home.join(format!(
        "{SEED_FILE}.tmp.{}.{nonce:016x}",
        std::process::id()
    ));
    let written = write_seed(&tmp, &fresh).and_then(|()| std::fs::rename(&tmp, &path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    // Read it back through the same checks, so a concurrent publisher cannot make us disagree.
    Ok(Identity::from_seed(read_seed(&path)?))
}

fn write_seed(tmp: &Path, seed: &[u8; 32]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(tmp)?;
    file.write_all(seed)?;
    file.sync_all()
}

/// Exactly 32 bytes from a regular, owner-only file we own. The checks use the open file.
fn read_seed(path: &Path) -> std::io::Result<[u8; 32]> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    let bad = |what: &str| std::io::Error::other(format!("{}: {what}", path.display()));
    if !meta.is_file() {
        return Err(bad("the system seed must be a regular file"));
    }
    if meta.uid() != crate::auth::daemon_uid() {
        return Err(bad("the system seed is owned by another user"));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(bad(
            "the system seed is accessible by other users; chmod 600",
        ));
    }
    let mut bytes = Vec::with_capacity(33);
    file.take(33).read_to_end(&mut bytes)?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| bad("the system seed is not 32 bytes"))
}

/// A fresh 16-byte nonce, hex encoded, from the OS random source.
fn fresh_nonce() -> std::io::Result<String> {
    let nonce = agora_crypto::random_bytes::<16>().map_err(std::io::Error::other)?;
    Ok(agora_store::hex(&nonce))
}

/// `SignedOp::new(kind, subject, arg, now_ms, random nonce).sign(id)`, or the error of the OS RNG when no nonce can be
/// drawn. Callers that can report the failure use this and treat it as a failed write.
pub fn try_sign(
    id: &Identity,
    now_ms: i64,
    kind: &str,
    subject: &str,
    arg: &str,
) -> std::io::Result<SignedOp> {
    let nonce = fresh_nonce()?;
    Ok(SignedOp::new(kind, subject, arg, now_ms, &nonce).sign(id))
}

/// As [`try_sign`], for callers that cannot take an error. When the OS RNG fails, the operation is signed with an
/// empty nonce, which the store refuses before any signature check (the nonce must be 1..=128 bytes). So the write
/// fails through the caller's existing error path, and nothing is recorded unsigned or with a reused nonce.
pub fn sign(id: &Identity, now_ms: i64, kind: &str, subject: &str, arg: &str) -> SignedOp {
    match try_sign(id, now_ms, kind, subject, arg) {
        Ok(op) => op,
        Err(e) => {
            tracing::error!(error = %e, kind, "the OS RNG failed; the operation will be refused by the store");
            SignedOp::new(kind, subject, arg, now_ms, "").sign(id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agora-sys-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp home");
        dir
    }

    #[test]
    fn creates_a_private_seed_and_reloads_the_same_key() {
        let home = temp_home("create");
        let first = load_or_create(&home).expect("create");
        let meta = std::fs::symlink_metadata(home.join(SEED_FILE)).expect("seed file");
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(meta.len(), 32);
        let again = load_or_create(&home).expect("reload");
        assert_eq!(first.public(), again.public());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn each_signature_carries_its_own_32_digit_nonce() {
        let key = Identity::from_seed([9; 32]);
        let a = try_sign(&key, 1, "job.note", "t", "{}").expect("sign");
        let b = sign(&key, 1, "job.note", "t", "{}");
        assert_eq!(a.nonce.len(), 32);
        assert!(!b.nonce.is_empty());
        assert_ne!(a.nonce, b.nonce, "a nonce is never reused");
        assert_eq!(a.actor, key.public());
    }

    #[test]
    fn refuses_a_symlinked_a_loose_or_a_short_seed() {
        let home = temp_home("refuse");
        let target = home.join("elsewhere");
        std::fs::write(&target, [7u8; 32]).expect("write");
        std::os::unix::fs::symlink(&target, home.join(SEED_FILE)).expect("symlink");
        assert!(load_or_create(&home).is_err());
        std::fs::remove_file(home.join(SEED_FILE)).expect("rm link");

        std::fs::write(home.join(SEED_FILE), [7u8; 32]).expect("write");
        std::fs::set_permissions(home.join(SEED_FILE), std::fs::Permissions::from_mode(0o644))
            .expect("chmod");
        assert!(load_or_create(&home).is_err());

        std::fs::set_permissions(home.join(SEED_FILE), std::fs::Permissions::from_mode(0o600))
            .expect("chmod");
        std::fs::write(home.join(SEED_FILE), [7u8; 31]).expect("short");
        assert!(load_or_create(&home).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }
}
