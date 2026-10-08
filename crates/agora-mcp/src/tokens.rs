//! HTTP bearer tokens: `$AGORA_HOME/mcp-tokens.json` (mode 0600).
//!
//! Format `{"version":1,"tokens":[{"id":"t_<8 hex>","label":"...","token_sha256":"<hex64>",
//! "seed":"<b64 32 bytes>","created":<ms>}]}`. The token string is `"agora_" +
//! base64url_nopad(32 random bytes)`, shown once at creation; only its SHA-256 is stored.
//! SIGNING IN HTTP MODE IS SERVER-HELD: the seed of an entry is the private key the HTTP
//! front-end signs with on behalf of the remote agent (whoever reads the file or holds the
//! token can act as that identity).

use crate::McpError;
use agora_crypto::Identity;
use agora_daemon::Paths;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub struct TokenEntry {
    /// "t_" + 8 lowercase hex.
    pub id: String,
    pub label: String,
    /// SHA-256 of the token string, 64 lowercase hex.
    pub token_sha256: String,
    /// Standard base64 (padded) of the 32-byte identity seed.
    pub seed: String,
    /// ms since the Unix epoch.
    pub created: i64,
}

/// Prints the id, label and creation time; the seed and the hash never reach a log or a panic.
impl fmt::Debug for TokenEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenEntry")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("token_sha256", &"<redacted>")
            .field("seed", &"<redacted>")
            .field("created", &self.created)
            .finish()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenFile {
    pub version: u32,
    pub tokens: Vec<TokenEntry>,
}

/// `paths.home/mcp-tokens.json`.
pub fn default_path(paths: &Paths) -> PathBuf {
    paths.home.join("mcp-tokens.json")
}

/// Read the token file. A missing file is an empty v1 file. A symlink, or a file with
/// `mode & 0o077 != 0`, is a `McpError::Token` naming the path; so is invalid JSON.
pub fn load(path: &Path) -> Result<TokenFile, McpError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TokenFile {
                version: 1,
                tokens: Vec::new(),
            });
        }
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        return Err(McpError::Token(format!(
            "{}: token file must not be a symlink",
            path.display()
        )));
    }
    // Open without following links, then check the descriptor that is read, so the mode check
    // and the read see the same file. O_NONBLOCK keeps a FIFO from hanging the open.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() {
        return Err(McpError::Token(format!(
            "{}: token file must be a regular file",
            path.display()
        )));
    }
    if opened.permissions().mode() & 0o077 != 0 {
        return Err(McpError::Token(format!(
            "{} is accessible by other users; chmod 600",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    // The serde message can echo a value, and the file holds seeds, so report only the position.
    let f: TokenFile = serde_json::from_slice(&bytes).map_err(|e| {
        McpError::Token(format!(
            "{}: invalid token file JSON at line {} column {}",
            path.display(),
            e.line(),
            e.column()
        ))
    })?;
    if f.version != 1 {
        return Err(McpError::Token(format!(
            "{}: unsupported token file version {}",
            path.display(),
            f.version
        )));
    }
    Ok(f)
}

/// Write the file atomically: temp file (0600) in the same directory, fsync, rename, fsync the
/// directory.
pub fn save(path: &Path, f: &TokenFile) -> Result<(), McpError> {
    let json = serde_json::to_vec_pretty(f)?;
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let tmp = dir.join(format!(".mcp-tokens.json.{}.tmp", random_hex::<4>()?));
    let result = write_temp(&tmp, &json).and_then(|()| {
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    });
    if result.is_err() {
        // Best effort: the temp file may already be gone after a successful rename.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Create a token for `label` (1..=64 bytes, else `McpError::Token`): random token string,
/// random seed, id `t_<8 hex>`; load + append + save under the token file lock. Returns
/// `(token string, entry)`; the token string is not stored anywhere.
pub fn add(path: &Path, label: &str, now_ms: i64) -> Result<(String, TokenEntry), McpError> {
    let label_ok = (1..=64).contains(&label.len()) && !label.chars().any(is_unprintable);
    if !label_ok {
        return Err(McpError::Token(
            "label must be 1..=64 printable bytes".to_string(),
        ));
    }
    let token = format!("agora_{}", URL_SAFE_NO_PAD.encode(random::<32>()?));
    let seed = STANDARD.encode(random::<32>()?);
    let _lock = lock_token_file(path)?;
    let mut f = load(path)?;
    let entry = TokenEntry {
        id: fresh_id(&f)?,
        label: label.to_string(),
        token_sha256: agora_store::hex(&Sha256::digest(token.as_bytes())),
        seed,
        created: now_ms,
    };
    f.tokens.push(entry.clone());
    save(path, &f)?;
    Ok((token, entry))
}

/// Remove the entry with `id` under the token file lock; `Ok(false)` when there is none.
pub fn revoke(path: &Path, id: &str) -> Result<bool, McpError> {
    let _lock = lock_token_file(path)?;
    let mut f = load(path)?;
    let before = f.tokens.len();
    f.tokens.retain(|t| t.id != id);
    let removed = f.tokens.len() != before;
    if removed {
        save(path, &f)?;
    }
    Ok(removed)
}

/// The entry whose `token_sha256` equals the SHA-256 of `bearer`. Compares every entry's hash
/// (constant-time per comparison, no early exit) so timing does not reveal which entry matched.
pub fn verify<'a>(f: &'a TokenFile, bearer: &str) -> Option<&'a TokenEntry> {
    let want = agora_store::hex(&Sha256::digest(bearer.as_bytes()));
    let mut found = None;
    for e in &f.tokens {
        let matched = hash_eq(&e.token_sha256, &want);
        found = found.or(matched.then_some(e));
    }
    found
}

/// The identity derived from the entry's seed (`McpError::Token` when the seed is not 32
/// base64 bytes).
pub fn identity_of(e: &TokenEntry) -> Result<Identity, McpError> {
    let malformed = || McpError::Token(format!("token entry {} has a malformed seed", e.id));
    let bytes = STANDARD.decode(&e.seed).map_err(|_| malformed())?;
    let seed: [u8; 32] = bytes.try_into().map_err(|_| malformed())?;
    Ok(Identity::from_seed(seed))
}

/// Byte-wise equality with no early exit: every byte pair is folded into one accumulator.
fn hash_eq(a: &str, b: &str) -> bool {
    let mut diff = u8::from(a.len() != b.len());
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// A fresh `t_<8 hex>` id that no entry of `f` uses.
fn fresh_id(f: &TokenFile) -> Result<String, McpError> {
    for _ in 0..16 {
        let id = format!("t_{}", random_hex::<4>()?);
        if f.tokens.iter().all(|t| t.id != id) {
            return Ok(id);
        }
    }
    Err(McpError::Token(
        "could not allocate a unique token id".to_string(),
    ))
}

fn random<const N: usize>() -> Result<[u8; N], McpError> {
    agora_crypto::random_bytes::<N>().map_err(|e| McpError::Token(e.to_string()))
}

fn random_hex<const N: usize>() -> Result<String, McpError> {
    Ok(agora_store::hex(&random::<N>()?))
}

/// Take the exclusive lock (`flock(LOCK_EX)`) on `<path>.lock`, created 0600 and never followed
/// through a symlink. The lock is held while the returned file is alive, so `add` and `revoke`
/// cannot interleave their load-modify-save.
fn lock_token_file(path: &Path) -> Result<std::fs::File, McpError> {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(name)?;
    file.lock()?;
    Ok(file)
}

/// Control characters, plus the characters that can make a label read differently from what it
/// is: zero-width and bidi controls, line and paragraph separators, Arabic letter mark,
/// Mongolian vowel separator, the BOM, variation selectors, and the Unicode tag characters
/// (which can spell hidden text).
fn is_unprintable(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{061C}'
                | '\u{17B4}'..='\u{17B5}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E007F}'
                | '\u{E0100}'..='\u{E01EF}'
        )
}

/// Create `tmp` exclusively with mode 0600, write `bytes`, and fsync it.
fn write_temp(tmp: &Path, bytes: &[u8]) -> Result<(), McpError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn token_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("mcp-tokens.json")
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    fn expect_token_error(result: Result<TokenFile, McpError>) -> String {
        match result {
            Err(McpError::Token(m)) => m,
            other => panic!("expected a token error, got {other:?}"),
        }
    }

    #[test]
    fn default_path_is_under_home() {
        let paths = Paths::new(Path::new("/tmp/agora-home"));
        assert_eq!(
            default_path(&paths),
            PathBuf::from("/tmp/agora-home/mcp-tokens.json")
        );
    }

    #[test]
    fn missing_file_loads_as_empty_v1() {
        let dir = tempfile::tempdir().expect("temp dir");
        let f = load(&token_path(&dir)).expect("load");
        assert_eq!(f.version, 1);
        assert!(f.tokens.is_empty());
    }

    #[test]
    fn add_then_load_round_trips_at_mode_0600() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = token_path(&dir);
        let (token, entry) = add(&path, "laptop", 1_700_000_000_000).expect("add");
        let f = load(&path).expect("load");
        assert_eq!(f.tokens.len(), 1);
        assert_eq!(f.tokens[0].id, entry.id);
        assert_eq!(f.tokens[0].label, "laptop");
        assert_eq!(f.tokens[0].created, 1_700_000_000_000);
        assert_eq!(mode_of(&path), 0o600);
        assert!(token.starts_with("agora_"));
    }

    #[test]
    fn token_starts_with_prefix_and_verify_finds_only_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = token_path(&dir);
        let (token, entry) = add(&path, "laptop", 1).expect("add");
        let f = load(&path).expect("load");
        assert_eq!(
            verify(&f, &token).map(|e| e.id.as_str()),
            Some(entry.id.as_str())
        );
        assert!(verify(&f, "agora_wrong").is_none());
        let mut almost = token.clone();
        almost.pop();
        almost.push('x');
        assert!(verify(&f, &almost).is_none());
    }

    #[test]
    fn two_tokens_each_map_to_their_own_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = token_path(&dir);
        let (first, a) = add(&path, "first", 1).expect("add first");
        let (second, b) = add(&path, "second", 2).expect("add second");
        assert_ne!(a.id, b.id);
        let f = load(&path).expect("load");
        assert_eq!(
            verify(&f, &first).map(|e| e.id.as_str()),
            Some(a.id.as_str())
        );
        assert_eq!(
            verify(&f, &second).map(|e| e.id.as_str()),
            Some(b.id.as_str())
        );
    }

    #[test]
    fn revoke_removes_entry_and_reports_unknown_ids() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = token_path(&dir);
        let (token, entry) = add(&path, "laptop", 1).expect("add");
        assert!(revoke(&path, &entry.id).expect("revoke"));
        let f = load(&path).expect("load");
        assert!(f.tokens.is_empty());
        assert!(verify(&f, &token).is_none());
        assert!(!revoke(&path, &entry.id).expect("second revoke"));
        let other = tempfile::tempdir().expect("other dir");
        assert!(!revoke(&token_path(&other), "t_00000000").expect("missing file"));
    }

    #[test]
    fn group_readable_file_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = token_path(&dir);
        add(&path, "laptop", 1).expect("add");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");
        let m = expect_token_error(load(&path));
        assert!(m.contains("mcp-tokens.json"), "{m}");
        assert!(m.contains("chmod 600"), "{m}");
    }

    #[test]
    fn symlinked_token_file_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("real.json");
        std::fs::write(&target, br#"{"version":1,"tokens":[]}"#).expect("write target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let path = token_path(&dir);
        std::os::unix::fs::symlink(&target, &path).expect("symlink");
        let m = expect_token_error(load(&path));
        assert!(m.contains("symlink"), "{m}");
    }

    #[test]
    fn bad_json_is_refused_with_the_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = token_path(&dir);
        std::fs::write(&path, b"{not json").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let m = expect_token_error(load(&path));
        assert!(m.contains("mcp-tokens.json"), "{m}");
    }

    #[test]
    fn unsupported_version_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = token_path(&dir);
        std::fs::write(&path, br#"{"version":2,"tokens":[]}"#).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let m = expect_token_error(load(&path));
        assert!(m.contains("version 2"), "{m}");
    }

    #[test]
    fn malformed_seed_is_refused() {
        let entry = |seed: String| TokenEntry {
            id: "t_0badseed".to_string(),
            label: "bad".to_string(),
            token_sha256: "0".repeat(64),
            seed,
            created: 1,
        };
        let not_base64 = identity_of(&entry("not base64!!".to_string()));
        assert!(matches!(not_base64, Err(McpError::Token(m)) if m.contains("t_0badseed")));
        let short = identity_of(&entry(STANDARD.encode([7u8; 31])));
        assert!(matches!(short, Err(McpError::Token(_))));
    }

    #[test]
    fn identity_of_is_deterministic_and_uses_the_stored_seed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = token_path(&dir);
        let (_, entry) = add(&path, "laptop", 1).expect("add");
        let first = identity_of(&entry).expect("identity").public();
        let second = identity_of(&entry).expect("identity").public();
        assert_eq!(first, second);
        let seed: [u8; 32] = STANDARD
            .decode(&entry.seed)
            .expect("seed b64")
            .try_into()
            .expect("32 bytes");
        assert_eq!(first, Identity::from_seed(seed).public());
    }

    #[test]
    fn token_string_never_appears_in_the_saved_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = token_path(&dir);
        let (token, entry) = add(&path, "laptop", 1).expect("add");
        let raw = String::from_utf8_lossy(&std::fs::read(&path).expect("read")).into_owned();
        assert!(!raw.contains(&token));
        assert!(raw.contains(&entry.token_sha256));
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = token_path(&dir);
        add(&path, "laptop", 1).expect("add");
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        // The lock file is by design; any temp file would show up as a third name.
        assert_eq!(
            names,
            vec![
                "mcp-tokens.json".to_string(),
                "mcp-tokens.json.lock".to_string()
            ]
        );
    }

    #[test]
    fn bad_labels_are_rejected_without_writing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = token_path(&dir);
        let too_long = "x".repeat(65);
        for label in [
            "",
            too_long.as_str(),
            "bad\nlabel",
            "tab\there",
            "nul\u{0}",
            "bidi\u{202E}override",
            "isolate\u{2066}x",
            "zero\u{200B}width",
            "mark\u{200F}x",
            "bom\u{FEFF}x",
            "line\u{2028}sep",
            "tag\u{E0041}x",
        ] {
            let err = add(&path, label, 1).expect_err("label refused");
            assert!(
                matches!(&err, McpError::Token(m) if m == "label must be 1..=64 printable bytes"),
                "{label:?}: {err:?}"
            );
        }
        assert!(!path.exists());
        let max = "x".repeat(64);
        assert!(add(&path, &max, 1).is_ok());
    }

    #[test]
    fn unprintable_label_characters_are_classed_as_such() {
        for c in [
            '\u{0}',
            '\u{7F}',
            '\u{200B}',
            '\u{200F}',
            '\u{202A}',
            '\u{202E}',
            '\u{2028}',
            '\u{2029}',
            '\u{2066}',
            '\u{2069}',
            '\u{206A}',
            '\u{2060}',
            '\u{061C}',
            '\u{180E}',
            '\u{00AD}',
            '\u{FEFF}',
            '\u{FE0F}',
            '\u{FFF9}',
            '\u{E0041}',
            '\u{E0100}',
        ] {
            assert!(is_unprintable(c), "{c:?} should be refused");
        }
        for c in ['a', 'é', '日', '😀', ' '] {
            assert!(!is_unprintable(c), "{c:?} should be allowed");
        }
    }

    #[test]
    fn debug_output_redacts_seed_and_hash() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (_, entry) = add(&token_path(&dir), "laptop", 42).expect("add");
        let file = TokenFile {
            version: 1,
            tokens: vec![entry.clone()],
        };
        for shown in [format!("{entry:?}"), format!("{file:?}")] {
            assert!(shown.contains(&entry.id), "{shown}");
            assert!(shown.contains("laptop"), "{shown}");
            assert!(shown.contains("<redacted>"), "{shown}");
            assert!(!shown.contains(&entry.seed), "{shown}");
            assert!(!shown.contains(&entry.token_sha256), "{shown}");
        }
    }

    #[test]
    fn lock_file_is_created_with_mode_0600() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = token_path(&dir);
        add(&path, "laptop", 1).expect("add");
        let lock = dir.path().join("mcp-tokens.json.lock");
        assert_eq!(mode_of(&lock), 0o600);
    }

    /// Adds on one thread and revokes on another, both through the lock: the load-modify-save
    /// of each is atomic, so no added entry is lost and every revoke takes effect.
    #[test]
    fn concurrent_add_and_revoke_never_lose_an_entry() {
        const ROUNDS: usize = 16;
        let dir = tempfile::tempdir().expect("temp dir");
        let path = token_path(&dir);
        let mut doomed = Vec::with_capacity(ROUNDS);
        for i in 0..ROUNDS {
            let (_, entry) = add(&path, &format!("doomed-{i}"), 1).expect("seed an entry");
            doomed.push(entry.id);
        }
        std::thread::scope(|scope| {
            let adder = scope.spawn(|| {
                for i in 0..ROUNDS {
                    add(&path, &format!("kept-{i}"), 2).expect("add");
                }
            });
            let reaper = scope.spawn(|| {
                for id in &doomed {
                    assert!(revoke(&path, id).expect("revoke"), "{id} was in the file");
                }
            });
            adder.join().expect("the adder thread does not panic");
            reaper.join().expect("the reaper thread does not panic");
        });
        let f = load(&path).expect("load");
        let kept = f
            .tokens
            .iter()
            .filter(|t| t.label.starts_with("kept-"))
            .count();
        assert_eq!(kept, ROUNDS, "an add was lost");
        assert_eq!(f.tokens.len(), ROUNDS, "a revoke was lost");
    }
}
