//! Per-folder identities. The only stored secret is the machine seed
//! (`$AGORA_HOME/machine.seed`, 32 random bytes, mode 0600). Each folder's Ed25519
//! seed is HKDF-SHA256(salt = "agora/folder-identity/v1", ikm = machine seed,
//! info = canonical absolute folder path bytes), so the same folder always gets the
//! same key and different folders get unrelated keys.

use crate::CliError;
use agora_crypto::Identity;
use agora_daemon::Paths;
use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const FOLDER_SALT: &[u8] = b"agora/folder-identity/v1";

/// Read `paths.machine_seed`, or create it: ensure `paths.home` exists with mode
/// 0700. If the seed exists, read it. Otherwise write 32 fresh bytes from
/// agora_crypto::random_bytes to a unique temp file in the same directory (mode
/// 0600, synced), then hard-link it to `paths.machine_seed`. The link is atomic and
/// the temp file is complete before it is linked, so no process can observe a
/// partly written seed: if the link finds the seed already present (AlreadyExists),
/// a concurrent process won and its complete seed is read. The temp file is removed
/// either way. A file that is not exactly 32 bytes -> Usage("corrupt machine seed at <path>").
pub fn load_or_create_machine_seed(paths: &Paths) -> Result<[u8; 32], CliError> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&paths.home)?;
    // The mode passed to DirBuilder is filtered by the umask; set it exactly.
    std::fs::set_permissions(&paths.home, Permissions::from_mode(0o700))?;

    match read_seed(&paths.machine_seed) {
        Ok(seed) => return Ok(seed),
        Err(CliError::Io(e)) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    let fresh = agora_crypto::random_bytes::<32>()?;
    let nonce = u64::from_le_bytes(agora_crypto::random_bytes::<8>()?);
    let tmp = paths.machine_seed.with_file_name(format!(
        "machine.seed.tmp.{}.{nonce:016x}",
        std::process::id()
    ));
    let published = publish_seed(&tmp, &paths.home, &paths.machine_seed, &fresh);
    // Best effort: the temp name is only ever a leftover, never read.
    let _ = std::fs::remove_file(&tmp);
    published
}

/// Write `seed` to `tmp` (create_new, mode 0600, synced), then hard-link it to
/// `target`, and sync `dir` (the directory holding `target`) so the new entry is durable.
/// Ok(our bytes) if we created the link; if `target` already exists another process
/// published a complete seed, so read it.
fn publish_seed(
    tmp: &Path,
    dir: &Path,
    target: &Path,
    seed: &[u8; 32],
) -> Result<[u8; 32], CliError> {
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(tmp)?;
        file.write_all(seed)?;
        file.sync_all()?;
    }
    match std::fs::hard_link(tmp, target) {
        Ok(()) => {
            std::fs::File::open(dir)?.sync_all()?;
            Ok(*seed)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => read_seed(target),
        Err(e) => Err(CliError::Usage(format!(
            "cannot create machine seed {}: {e}; the filesystem must support hard links",
            target.display()
        ))),
    }
}

/// Read exactly 32 bytes, then require that no group or other user can read the file.
/// At most 33 bytes are read, so an oversized file is rejected without being loaded
/// whole. The mode is taken from the open file, so it is the file that was read.
fn read_seed(path: &Path) -> Result<[u8; 32], CliError> {
    let file = std::fs::File::open(path)?;
    let mode = file.metadata()?.permissions().mode();
    let mut bytes = Vec::with_capacity(33);
    file.take(33).read_to_end(&mut bytes)?;
    let seed = <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| CliError::Usage(format!("corrupt machine seed at {}", path.display())))?;
    if mode & 0o077 != 0 {
        return Err(CliError::Usage(format!(
            "machine seed {} is accessible by other users; chmod 600",
            path.display()
        )));
    }
    Ok(seed)
}

/// HKDF-SHA256 per the module doc; info = folder.as_os_str().as_bytes().
pub fn folder_seed(machine: &[u8; 32], folder: &Path) -> [u8; 32] {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(FOLDER_SALT), machine);
    let mut out = [0u8; 32];
    hk.expand(folder.as_os_str().as_bytes(), &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 length");
    out
}

/// `$AGORA_FOLDER` if set and non-empty, else the current directory; canonicalized.
pub fn current_folder() -> Result<PathBuf, CliError> {
    let raw = match std::env::var_os("AGORA_FOLDER") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => std::env::current_dir()?,
    };
    raw.canonicalize().map_err(|e| {
        CliError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", raw.display()),
        ))
    })
}

/// (identity for current_folder(), label = the folder's file name or "root").
pub fn load_identity(paths: &Paths) -> Result<(Identity, String), CliError> {
    let machine = load_or_create_machine_seed(paths)?;
    let folder = current_folder()?;
    let identity = Identity::from_seed(folder_seed(&machine, &folder));
    let label = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_owned());
    Ok((identity, label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A unique directory under the temp dir, removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("agora-identity-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    #[test]
    fn same_folder_same_key() {
        let machine = [7u8; 32];
        let folder = Path::new("/work/a");
        assert_eq!(folder_seed(&machine, folder), folder_seed(&machine, folder));
    }

    #[test]
    fn different_folders_different_keys() {
        let machine = [7u8; 32];
        assert_ne!(
            folder_seed(&machine, Path::new("/work/a")),
            folder_seed(&machine, Path::new("/work/b"))
        );
    }

    #[test]
    fn different_machine_seeds_different_keys() {
        let folder = Path::new("/work/a");
        assert_ne!(
            folder_seed(&[1u8; 32], folder),
            folder_seed(&[2u8; 32], folder)
        );
    }

    #[test]
    fn seed_file_is_0600_home_is_0700_and_second_call_returns_same_seed() {
        let t = TempDir::new("create");
        let paths = Paths::new(&t.path().join("home"));
        let first = load_or_create_machine_seed(&paths).expect("create seed");
        let second = load_or_create_machine_seed(&paths).expect("read existing seed");
        assert_eq!(first, second);
        assert_eq!(mode(&paths.home), 0o700);
        assert_eq!(mode(&paths.machine_seed), 0o600);
        assert_eq!(
            std::fs::read(&paths.machine_seed)
                .expect("read seed file")
                .len(),
            32
        );
    }

    #[test]
    fn concurrent_first_calls_agree_on_one_seed_and_leave_no_temp_files() {
        let t = TempDir::new("concurrent");
        let paths = Paths::new(&t.path().join("home"));
        let barrier = std::sync::Barrier::new(8);
        let seeds: Vec<[u8; 32]> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let (paths, barrier) = (&paths, &barrier);
                    s.spawn(move || {
                        barrier.wait();
                        load_or_create_machine_seed(paths).expect("concurrent load")
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("thread finished"))
                .collect()
        });
        assert!(seeds.windows(2).all(|w| w[0] == w[1]), "seeds differ");
        assert_eq!(
            std::fs::read(&paths.machine_seed).expect("read seed file"),
            seeds[0]
        );
        let leftovers: Vec<_> = std::fs::read_dir(&paths.home)
            .expect("list home")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("machine.seed.tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    #[test]
    fn seed_file_readable_by_others_is_usage_error() {
        let t = TempDir::new("open-mode");
        let paths = Paths::new(t.path());
        std::fs::write(&paths.machine_seed, [9u8; 32]).expect("write seed");
        std::fs::set_permissions(&paths.machine_seed, Permissions::from_mode(0o644))
            .expect("chmod seed");
        let err = load_or_create_machine_seed(&paths).expect_err("0644 seed must be rejected");
        assert!(matches!(err, CliError::Usage(_)), "got {err:?}");
    }

    #[test]
    fn seed_file_of_31_bytes_is_usage_error() {
        let t = TempDir::new("short");
        let paths = Paths::new(t.path());
        std::fs::write(&paths.machine_seed, [9u8; 31]).expect("write short seed");
        let err = load_or_create_machine_seed(&paths).expect_err("31 bytes must be rejected");
        assert!(matches!(err, CliError::Usage(_)), "got {err:?}");
    }
}
