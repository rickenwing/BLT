//! Shared app state for the desktop client.

use crate::db;
use crate::downloads::DownloadManager;
use blt_core::protocol::{JukeboxState, Mode, PeerEntry, RosterEntry};
use blt_core::runtime::ComponentDirs;
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::Arc;

/// Durable settings (mirrored from the client DB at startup, written through).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    pub mode: Mode,
    pub playback_locked: bool,
    pub display_name: String,
    pub client_id: String,
    pub default_download_root: Option<String>,
    pub upload_cap_bytes_per_sec: u64,
    pub share_back: bool,
    pub last_server: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: Mode::Client,
            playback_locked: false,
            display_name: String::new(),
            client_id: String::new(),
            default_download_root: None,
            upload_cap_bytes_per_sec: blt_core::p2p::DEFAULT_UPLOAD_CAP_BPS,
            share_back: true,
            last_server: None,
        }
    }
}

impl Settings {
    pub fn load(conn: &Connection) -> Self {
        let get = |k: &str| db::get_setting(conn, k).ok().flatten();
        let mut s = Settings {
            mode: match get("mode").as_deref() {
                Some("playback") => Mode::Playback,
                _ => Mode::Client,
            },
            playback_locked: get("playback_locked").as_deref() == Some("true"),
            display_name: get("display_name").unwrap_or_default(),
            client_id: get("client_id").unwrap_or_default(),
            default_download_root: get("default_download_root"),
            upload_cap_bytes_per_sec: get("upload_cap_bytes_per_sec")
                .and_then(|v| v.parse().ok())
                .unwrap_or(blt_core::p2p::DEFAULT_UPLOAD_CAP_BPS),
            share_back: get("share_back").as_deref() != Some("false"),
            last_server: get("last_server"),
        };
        // First run: client id + display name defaulted from the computer name
        // (F7.1); persisted immediately so they survive restarts.
        if s.client_id.is_empty() {
            s.client_id = uuid::Uuid::new_v4().to_string();
            let _ = db::set_setting(conn, "client_id", &s.client_id);
        }
        if s.display_name.is_empty() {
            s.display_name = hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "Player".to_string());
            let _ = db::set_setting(conn, "display_name", &s.display_name);
        }
        s
    }

    pub fn persist(&self, conn: &Connection) -> rusqlite::Result<()> {
        let mode = match self.mode {
            Mode::Client => "client",
            Mode::Playback => "playback",
        };
        db::set_setting(conn, "mode", mode)?;
        db::set_setting(
            conn,
            "playback_locked",
            if self.playback_locked {
                "true"
            } else {
                "false"
            },
        )?;
        db::set_setting(conn, "display_name", &self.display_name)?;
        db::set_setting(conn, "client_id", &self.client_id)?;
        if let Some(root) = &self.default_download_root {
            db::set_setting(conn, "default_download_root", root)?;
        }
        db::set_setting(
            conn,
            "upload_cap_bytes_per_sec",
            &self.upload_cap_bytes_per_sec.to_string(),
        )?;
        db::set_setting(
            conn,
            "share_back",
            if self.share_back { "true" } else { "false" },
        )?;
        if let Some(s) = &self.last_server {
            db::set_setting(conn, "last_server", s)?;
        }
        Ok(())
    }
}

/// The connected server's per-service endpoints.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Connection_ {
    pub game_endpoint: Option<String>,
    pub share_endpoint: Option<String>,
    pub server_label: Option<String>,
    pub server_uuid: Option<String>,
    pub ws_connected: bool,
}

/// Latest live state received over `/ws` (consumed by the UI; resynced on
/// reconnect — HARD CONSTRAINT #12).
#[derive(Default)]
pub struct LiveState {
    pub jukebox: Option<JukeboxState>,
    pub roster: Vec<RosterEntry>,
    /// Last peer list per title (M4 scheduling input).
    pub peers: HashMap<(u64, u32), Vec<PeerEntry>>,
    /// Result of the reachability self-test; None = not yet probed (F13.4).
    pub p2p_reachable: Option<bool>,
}

pub struct ClientState {
    pub db: Mutex<Connection>,
    pub dirs: ComponentDirs,
    pub settings: RwLock<Settings>,
    pub connection: RwLock<Connection_>,
    pub live: RwLock<LiveState>,
    pub downloads: DownloadManager,
    /// Outgoing WS sender (set once the WS loop runs).
    pub ws_out: RwLock<Option<tokio::sync::mpsc::UnboundedSender<blt_core::protocol::ClientMsg>>>,
    /// Local chunk-server port when seeding (share_back on).
    pub seed_port: RwLock<Option<u16>>,
}

pub type Shared = Arc<ClientState>;

impl ClientState {
    pub fn send_ws(&self, msg: blt_core::protocol::ClientMsg) {
        if let Some(tx) = self.ws_out.read().as_ref() {
            let _ = tx.send(msg);
        }
    }
}
