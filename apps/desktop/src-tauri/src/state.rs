//! Shared app state for the desktop client.

use crate::db;
use crate::downloads::DownloadManager;
use blt_core::protocol::{JukeboxState, Mode, PeerEntry, RosterEntry};
use blt_core::runtime::ComponentDirs;
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tauri::{AppHandle, Emitter};

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
    /// Localhost media-proxy port (serves the YouTube embed page over http so
    /// the player gets a valid origin/Referer — YouTube Error 153 workaround).
    pub media_port: RwLock<Option<u16>>,
    /// App handle for emitting events (set in `setup`).
    pub app: OnceLock<AppHandle>,
    /// Active shared-pool transfers (uploads + downloads), keyed by id.
    pub xfers: Mutex<HashMap<u64, ShareXfer>>,
    pub xfer_seq: AtomicU64,
    /// Embedded mpv player state (jukebox local/LAN media playback).
    pub mpv: Mutex<crate::mpv::MpvPlayer>,
    /// Smoothed seed (upload) throughput — chunks served to peers (People column).
    pub seed_meter: Mutex<blt_core::ratemeter::RateMeter>,
    /// Last reported roster activity, re-sent periodically so an idle-but-seeding
    /// client keeps its seed speed fresh in the roster.
    pub last_activity: RwLock<String>,
}

pub type Shared = Arc<ClientState>;

/// One in-flight shared-pool transfer (games have their own DownloadManager).
pub struct ShareXfer {
    pub kind: &'static str, // "share-download" | "share-upload"
    pub label: String,
    pub total: u64,
    pub done: u64,
    pub speed_bps: u64,
    last_t: Instant,
    last_bytes: u64,
    /// Set by a cancel request; the streaming upload/download checks it and aborts.
    cancel: Arc<AtomicBool>,
}

/// Uniform transfer row for the sidebar/title-bar indicator (games + shares).
#[derive(serde::Serialize)]
pub struct TransferRow {
    pub id: String,
    pub kind: String, // "game-download" | "share-download" | "share-upload"
    pub label: String,
    pub done: u64,
    pub total: u64,
    pub speed_bps: u64,
}

impl ClientState {
    pub fn send_ws(&self, msg: blt_core::protocol::ClientMsg) {
        if let Some(tx) = self.ws_out.read().as_ref() {
            let _ = tx.send(msg);
        }
    }

    fn emit_transfers(&self) {
        if let Some(a) = self.app.get() {
            let _ = a.emit("transfers-changed", ());
        }
    }

    /// Register a new share transfer; returns its id.
    pub fn xfer_begin(&self, kind: &'static str, label: &str, total: u64) -> u64 {
        let id = self.xfer_seq.fetch_add(1, Ordering::SeqCst);
        self.xfers.lock().insert(
            id,
            ShareXfer {
                kind,
                label: label.to_string(),
                total,
                done: 0,
                speed_bps: 0,
                last_t: Instant::now(),
                last_bytes: 0,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        self.emit_transfers();
        id
    }

    /// Clone an in-flight transfer's cancel flag so the streaming upload/download
    /// can poll it and abort mid-file.
    pub fn xfer_cancel_flag(&self, id: u64) -> Option<Arc<AtomicBool>> {
        self.xfers.lock().get(&id).map(|x| x.cancel.clone())
    }

    /// Request cancellation of an in-flight share transfer.
    pub fn xfer_cancel(&self, id: u64) {
        if let Some(x) = self.xfers.lock().get(&id) {
            x.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Update bytes done; recomputes a smoothed speed at most twice a second.
    /// Cheap enough to call per chunk (no event emit here — the UI polls).
    pub fn xfer_update(&self, id: u64, done: u64) {
        let mut m = self.xfers.lock();
        if let Some(x) = m.get_mut(&id) {
            let dt = x.last_t.elapsed().as_secs_f64();
            if dt >= 0.5 {
                let inst = (done.saturating_sub(x.last_bytes) as f64 / dt) as u64;
                x.speed_bps = if x.speed_bps == 0 {
                    inst
                } else {
                    (x.speed_bps * 3 + inst * 2) / 5 // light EWMA
                };
                x.last_t = Instant::now();
                x.last_bytes = done;
            }
            x.done = done;
        }
    }

    /// Finish a share transfer (success or failure both just drop it).
    pub fn xfer_end(&self, id: u64) {
        self.xfers.lock().remove(&id);
        self.emit_transfers();
    }

    pub fn share_transfers(&self) -> Vec<TransferRow> {
        self.xfers
            .lock()
            .iter()
            .map(|(id, x)| TransferRow {
                id: format!("share-{id}"),
                kind: x.kind.to_string(),
                label: x.label.clone(),
                done: x.done,
                total: x.total,
                speed_bps: x.speed_bps,
            })
            .collect()
    }
}
