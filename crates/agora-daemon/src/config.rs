//! Daemon configuration and on-disk layout.

use crate::DaemonError;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Every file the daemon and the CLI agree on, all directly under `home`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
    /// home/agora.db
    pub db: PathBuf,
    /// home/agora.sock
    pub socket: PathBuf,
    /// home/agora.lock — flock held for the daemon's lifetime.
    pub lock: PathBuf,
    /// home/blobs — spilled blob files.
    pub blobs: PathBuf,
    /// home/spawn.lock — CLI auto-spawn serialization.
    pub spawn_lock: PathBuf,
    /// home/daemon.log — stderr of an auto-spawned daemon.
    pub log: PathBuf,
    /// home/machine.seed — 32 random bytes, 0600, the CLI's identity root.
    pub machine_seed: PathBuf,
    /// home/config.json — optional DaemonConfig overrides.
    pub config: PathBuf,
}

impl Paths {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            db: home.join("agora.db"),
            socket: home.join("agora.sock"),
            lock: home.join("agora.lock"),
            blobs: home.join("blobs"),
            spawn_lock: home.join("spawn.lock"),
            log: home.join("daemon.log"),
            machine_seed: home.join("machine.seed"),
            config: home.join("config.json"),
        }
    }
}

/// Upper bound for `max_blob_bytes`. A blob travels base64-encoded inside one frame of at most
/// 8 MiB. 6 MiB of blob is exactly 8 MiB of base64, which leaves no room for the JSON envelope
/// around the bytes (`id`, `req`, the field name), so the bound sits 64 KiB below 6 MiB.
const MAX_BLOB_BYTES_LIMIT: usize = 6 * 1024 * 1024 - 64 * 1024;

/// Policy knobs. `config.json` may set any subset (unknown keys are an error);
/// `home` is never read from the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    #[serde(skip)]
    pub home: PathBuf,
    /// Local mode: any same-uid key may `register`. Default true.
    pub open_registration: bool,
    /// Done gate needs an independent approval. Default true.
    pub require_review: bool,
    /// Largest blob accepted over the socket. Default 5 MiB (must fit a frame as base64).
    pub max_blob_bytes: usize,
    /// Total blob bytes one identity may upload. Default 256 MiB.
    pub blob_quota_bytes: i64,
    /// Concurrent connections; extra accepts are closed immediately. Default 256.
    pub max_connections: usize,
    /// Requests in flight per connection. Default 32.
    pub max_inflight_per_conn: usize,
    /// Handshake must finish within this. Default 10_000.
    pub handshake_timeout_ms: u64,
    /// Install SIGINT/SIGTERM handlers in `Daemon::run`. Default true (tests: false).
    pub handle_signals: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            home: DaemonConfig::default_home(),
            open_registration: true,
            require_review: true,
            max_blob_bytes: 5 * 1024 * 1024,
            blob_quota_bytes: 256 * 1024 * 1024,
            max_connections: 256,
            max_inflight_per_conn: 32,
            handshake_timeout_ms: 10_000,
            handle_signals: true,
        }
    }
}

impl DaemonConfig {
    /// `$AGORA_HOME` if set and non-empty, else `$HOME/.agora`.
    pub fn default_home() -> PathBuf {
        default_home_from(
            std::env::var_os("AGORA_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }

    /// Defaults with `home` set, overlaid by `home/config.json` when it exists.
    /// Malformed file -> `DaemonError::Config("config.json: <serde message>")`.
    pub fn load(home: &Path) -> Result<Self, DaemonError> {
        let path = Paths::new(home).config;
        let mut cfg = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<DaemonConfig>(&bytes)
                .map_err(|e| DaemonError::Config(format!("config.json: {e}")))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DaemonConfig::default(),
            Err(e) => return Err(DaemonError::Io(e)),
        };
        cfg.home = home.to_path_buf();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Rejects values the daemon cannot run with. `load` calls it after the overlay.
    /// Each message starts with the offending field name.
    pub fn validate(&self) -> Result<(), DaemonError> {
        if self.max_connections == 0 {
            return Err(DaemonError::Config(format!(
                "max_connections must be at least 1, got {}",
                self.max_connections
            )));
        }
        if self.max_inflight_per_conn == 0 {
            return Err(DaemonError::Config(format!(
                "max_inflight_per_conn must be at least 1, got {}",
                self.max_inflight_per_conn
            )));
        }
        if self.handshake_timeout_ms == 0 {
            return Err(DaemonError::Config(format!(
                "handshake_timeout_ms must be at least 1, got {}",
                self.handshake_timeout_ms
            )));
        }
        if self.blob_quota_bytes < 0 {
            return Err(DaemonError::Config(format!(
                "blob_quota_bytes must not be negative, got {}",
                self.blob_quota_bytes
            )));
        }
        if self.max_blob_bytes == 0 || self.max_blob_bytes > MAX_BLOB_BYTES_LIMIT {
            return Err(DaemonError::Config(format!(
                "max_blob_bytes must be between 1 and {MAX_BLOB_BYTES_LIMIT}, got {}",
                self.max_blob_bytes
            )));
        }
        Ok(())
    }

    pub fn paths(&self) -> Paths {
        Paths::new(&self.home)
    }
}

/// [`DaemonConfig::default_home`] with the two environment values passed in, so the
/// fallback order can be tested without touching the process environment.
fn default_home_from(agora_home: Option<&OsStr>, home: Option<&OsStr>) -> PathBuf {
    if let Some(dir) = agora_home.filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    match home.filter(|h| !h.is_empty()) {
        Some(h) => PathBuf::from(h).join(".agora"),
        None => PathBuf::from(".agora"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique directory under the temp dir, removed when dropped.
    struct TempHome(PathBuf);

    impl TempHome {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("agora-config-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp home");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write_config(&self, body: &str) {
            std::fs::write(self.0.join("config.json"), body).expect("write config.json");
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn paths_layout_lives_directly_under_home() {
        let p = Paths::new(Path::new("/h"));
        assert_eq!(p.home, PathBuf::from("/h"));
        assert_eq!(p.db, PathBuf::from("/h/agora.db"));
        assert_eq!(p.socket, PathBuf::from("/h/agora.sock"));
        assert_eq!(p.lock, PathBuf::from("/h/agora.lock"));
        assert_eq!(p.blobs, PathBuf::from("/h/blobs"));
        assert_eq!(p.spawn_lock, PathBuf::from("/h/spawn.lock"));
        assert_eq!(p.log, PathBuf::from("/h/daemon.log"));
        assert_eq!(p.machine_seed, PathBuf::from("/h/machine.seed"));
        assert_eq!(p.config, PathBuf::from("/h/config.json"));
    }

    #[test]
    fn default_config_has_documented_values() {
        let cfg = DaemonConfig::default();
        assert_eq!(cfg.home, DaemonConfig::default_home());
        assert!(cfg.open_registration);
        assert!(cfg.require_review);
        assert_eq!(cfg.max_blob_bytes, 5 * 1024 * 1024);
        assert_eq!(cfg.blob_quota_bytes, 256 * 1024 * 1024);
        assert_eq!(cfg.max_connections, 256);
        assert_eq!(cfg.max_inflight_per_conn, 32);
        assert_eq!(cfg.handshake_timeout_ms, 10_000);
        assert!(cfg.handle_signals);
    }

    #[test]
    fn default_home_prefers_non_empty_agora_home() {
        assert_eq!(
            default_home_from(Some(OsStr::new("/srv/agora")), Some(OsStr::new("/home/u"))),
            PathBuf::from("/srv/agora")
        );
    }

    #[test]
    fn default_home_empty_agora_home_falls_back_to_home() {
        assert_eq!(
            default_home_from(Some(OsStr::new("")), Some(OsStr::new("/home/u"))),
            PathBuf::from("/home/u/.agora")
        );
        assert_eq!(
            default_home_from(None, Some(OsStr::new("/home/u"))),
            PathBuf::from("/home/u/.agora")
        );
    }

    #[test]
    fn default_home_without_env_is_relative_dot_agora() {
        assert_eq!(default_home_from(None, None), PathBuf::from(".agora"));
    }

    #[test]
    fn load_without_file_returns_defaults_with_home() {
        let t = TempHome::new("no-file");
        let cfg = DaemonConfig::load(t.path()).expect("load without config.json");
        assert_eq!(
            cfg,
            DaemonConfig {
                home: t.path().to_path_buf(),
                ..DaemonConfig::default()
            }
        );
    }

    #[test]
    fn load_partial_file_overrides_only_given_fields() {
        let t = TempHome::new("partial");
        t.write_config(r#"{"max_connections": 8, "require_review": false}"#);
        let cfg = DaemonConfig::load(t.path()).expect("load partial config.json");
        assert_eq!(
            cfg,
            DaemonConfig {
                home: t.path().to_path_buf(),
                max_connections: 8,
                require_review: false,
                ..DaemonConfig::default()
            }
        );
    }

    #[test]
    fn load_unknown_key_is_config_error() {
        let t = TempHome::new("unknown-key");
        t.write_config(r#"{"max_conections": 8}"#);
        let err = DaemonConfig::load(t.path()).expect_err("unknown key must be rejected");
        assert!(
            matches!(&err, DaemonError::Config(m) if m.starts_with("config.json: ")),
            "got {err:?}"
        );
    }

    /// The message of a config that `validate` rejects; panics on any other outcome.
    fn rejected(cfg: &DaemonConfig) -> String {
        match cfg.validate() {
            Err(DaemonError::Config(m)) => m,
            other => panic!("expected a Config error, got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_defaults() {
        DaemonConfig::default()
            .validate()
            .expect("default config is valid");
    }

    #[test]
    fn validate_rejects_zero_max_connections() {
        let cfg = DaemonConfig {
            max_connections: 0,
            ..DaemonConfig::default()
        };
        assert!(rejected(&cfg).starts_with("max_connections "));
    }

    #[test]
    fn validate_rejects_zero_max_inflight_per_conn() {
        let cfg = DaemonConfig {
            max_inflight_per_conn: 0,
            ..DaemonConfig::default()
        };
        assert!(rejected(&cfg).starts_with("max_inflight_per_conn "));
    }

    #[test]
    fn validate_rejects_zero_handshake_timeout() {
        let cfg = DaemonConfig {
            handshake_timeout_ms: 0,
            ..DaemonConfig::default()
        };
        assert!(rejected(&cfg).starts_with("handshake_timeout_ms "));
    }

    #[test]
    fn validate_rejects_negative_blob_quota_and_accepts_zero() {
        let negative = DaemonConfig {
            blob_quota_bytes: -1,
            ..DaemonConfig::default()
        };
        assert!(rejected(&negative).starts_with("blob_quota_bytes "));
        let zero = DaemonConfig {
            blob_quota_bytes: 0,
            ..DaemonConfig::default()
        };
        zero.validate().expect("a zero quota is allowed");
    }

    #[test]
    fn validate_rejects_zero_max_blob_bytes() {
        let cfg = DaemonConfig {
            max_blob_bytes: 0,
            ..DaemonConfig::default()
        };
        assert!(rejected(&cfg).starts_with("max_blob_bytes "));
    }

    #[test]
    fn validate_caps_max_blob_bytes_below_the_frame_limit() {
        let at_limit = DaemonConfig {
            max_blob_bytes: 6 * 1024 * 1024 - 64 * 1024,
            ..DaemonConfig::default()
        };
        at_limit.validate().expect("6 MiB less 64 KiB is allowed");
        let over = DaemonConfig {
            max_blob_bytes: 6 * 1024 * 1024 - 64 * 1024 + 1,
            ..DaemonConfig::default()
        };
        assert!(rejected(&over).starts_with("max_blob_bytes "));
    }

    #[test]
    fn load_rejects_invalid_values_from_config_json() {
        let t = TempHome::new("invalid-value");
        t.write_config(r#"{"max_connections": 0}"#);
        let err = DaemonConfig::load(t.path()).expect_err("zero connections must be rejected");
        assert!(
            matches!(&err, DaemonError::Config(m) if m.starts_with("max_connections ")),
            "got {err:?}"
        );
    }

    #[test]
    fn load_malformed_json_is_config_error() {
        let t = TempHome::new("malformed");
        t.write_config(r#"{"max_connections": "#);
        let err = DaemonConfig::load(t.path()).expect_err("malformed JSON must be rejected");
        assert!(
            matches!(&err, DaemonError::Config(m) if m.starts_with("config.json: ")),
            "got {err:?}"
        );
    }
}
