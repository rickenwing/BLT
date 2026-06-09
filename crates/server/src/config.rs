//! Server configuration (TDD §3.1, §4.1).
//!
//! `config.toml` is the **authoritative, hand-editable** store so a bad admin
//! binding is recoverable by editing the file + restart (F1.4, HARD CONSTRAINT
//! recovery). The admin panel edits rewrite this file. Per-service binds let the
//! three services spread across up to three NICs or collapse onto one (F1.1-.2).

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Default ports (TDD §3.1).
pub const DEFAULT_GAME_PORT: u16 = 7400;
pub const DEFAULT_SHARE_PORT: u16 = 7401;
pub const DEFAULT_ADMIN_PORT: u16 = 7402;

fn default_game_bind() -> String {
    format!("0.0.0.0:{DEFAULT_GAME_PORT}")
}
fn default_share_bind() -> String {
    format!("0.0.0.0:{DEFAULT_SHARE_PORT}")
}
fn default_admin_bind() -> String {
    // The admin persona reaches the panel from their own laptop (TDD §3.1:
    // "configurable NIC / all"), so the default is LAN-reachable; the panel is
    // password-gated (F1.6) and rebindable to one NIC or localhost.
    format!("0.0.0.0:{DEFAULT_ADMIN_PORT}")
}
fn default_chunk_size() -> u64 {
    blt_core::DEFAULT_CHUNK_SIZE
}
fn default_settle() -> u64 {
    30
}
fn default_scan_interval() -> u64 {
    300
}
fn default_backup_interval() -> u64 {
    600
}
fn default_peer_timeout() -> u64 {
    15
}
fn default_order_mode() -> String {
    "fair".into()
}

/// The full server configuration, serialised to `config.toml`.
#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_game_bind")]
    pub game_distribution_bind: String,
    #[serde(default = "default_share_bind")]
    pub shared_pool_bind: String,
    #[serde(default = "default_admin_bind")]
    pub admin_panel_bind: String,

    #[serde(default)]
    pub library_path: Option<PathBuf>,
    #[serde(default)]
    pub staging_path: Option<PathBuf>,
    #[serde(default)]
    pub share_path: Option<PathBuf>,

    #[serde(default = "default_chunk_size")]
    pub chunk_size: u64,
    #[serde(default = "default_settle")]
    pub staging_settle_secs: u64,
    #[serde(default = "default_scan_interval")]
    pub scan_interval_secs: u64,
    #[serde(default = "default_backup_interval")]
    pub db_backup_interval_secs: u64,
    #[serde(default = "default_peer_timeout")]
    pub peer_timeout_secs: u64,

    #[serde(default = "default_order_mode")]
    pub jukebox_order_mode: String,

    #[serde(default)]
    pub server_uuid: String,
    #[serde(default = "default_label")]
    pub server_label: String,

    /// Argon2 hash of the admin password (set on first run, F1.6). `None` until set.
    #[serde(default)]
    pub admin_password_hash: Option<String>,
}

fn default_label() -> String {
    "Buttz LAN Tool".into()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            game_distribution_bind: default_game_bind(),
            shared_pool_bind: default_share_bind(),
            admin_panel_bind: default_admin_bind(),
            library_path: None,
            staging_path: None,
            share_path: None,
            chunk_size: default_chunk_size(),
            staging_settle_secs: default_settle(),
            scan_interval_secs: default_scan_interval(),
            db_backup_interval_secs: default_backup_interval(),
            peer_timeout_secs: default_peer_timeout(),
            jukebox_order_mode: default_order_mode(),
            server_uuid: String::new(),
            server_label: default_label(),
            admin_password_hash: None,
        }
    }
}

// Manual Debug: the admin password hash must never leak into logs via a casual
// `{:?}` (HARD CONSTRAINT #16 — never log secrets).
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("game_distribution_bind", &self.game_distribution_bind)
            .field("shared_pool_bind", &self.shared_pool_bind)
            .field("admin_panel_bind", &self.admin_panel_bind)
            .field("library_path", &self.library_path)
            .field("staging_path", &self.staging_path)
            .field("share_path", &self.share_path)
            .field("chunk_size", &self.chunk_size)
            .field("staging_settle_secs", &self.staging_settle_secs)
            .field("scan_interval_secs", &self.scan_interval_secs)
            .field("db_backup_interval_secs", &self.db_backup_interval_secs)
            .field("peer_timeout_secs", &self.peer_timeout_secs)
            .field("jukebox_order_mode", &self.jukebox_order_mode)
            .field("server_uuid", &self.server_uuid)
            .field("server_label", &self.server_label)
            .field(
                "admin_password_hash",
                &self.admin_password_hash.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl Config {
    /// Load from `path`, creating a default file (with a fresh server UUID) if
    /// absent. Always ensures a `server_uuid` is present.
    pub fn load_or_create(path: &Path) -> anyhow::Result<Self> {
        let mut cfg = if path.exists() {
            let text = std::fs::read_to_string(path)?;
            toml::from_str(&text)?
        } else {
            Config::default()
        };
        if cfg.server_uuid.is_empty() {
            cfg.server_uuid = uuid::Uuid::new_v4().to_string();
        }
        cfg.save(path)?;
        Ok(cfg)
    }

    /// Persist to `config.toml` (authoritative + recovery-friendly).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    pub fn game_bind(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.game_distribution_bind.parse()?)
    }
    pub fn share_bind(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.shared_pool_bind.parse()?)
    }
    pub fn admin_bind(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.admin_panel_bind.parse()?)
    }

    pub fn order_mode(&self) -> blt_core::jukebox::OrderMode {
        blt_core::jukebox::OrderMode::from_str_lossy(&self.jukebox_order_mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_creates_default_with_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config::load_or_create(&path).unwrap();
        assert!(path.exists());
        assert!(!cfg.server_uuid.is_empty());
        assert_eq!(cfg.chunk_size, blt_core::DEFAULT_CHUNK_SIZE);
        assert!(cfg.admin_panel_bind.ends_with("7402"));

        // reload keeps the same uuid
        let cfg2 = Config::load_or_create(&path).unwrap();
        assert_eq!(cfg.server_uuid, cfg2.server_uuid);
    }

    #[test]
    fn binds_parse() {
        let cfg = Config::default();
        assert_eq!(cfg.game_bind().unwrap().port(), DEFAULT_GAME_PORT);
        assert_eq!(cfg.admin_bind().unwrap().port(), DEFAULT_ADMIN_PORT);
        // LAN-reachable by default; password-gated and rebindable (F1.1/F1.6).
        assert!(cfg.admin_bind().unwrap().ip().is_unspecified());
    }

    #[test]
    fn debug_never_exposes_password_hash() {
        let cfg = Config {
            admin_password_hash: Some("$argon2id$super-secret".into()),
            ..Default::default()
        };
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("super-secret"));
        assert!(dbg.contains("<redacted>"));
    }
}
