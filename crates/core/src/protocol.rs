//! Wire types shared by the server, the desktop app, and the admin SPA.
//!
//! Two decoupled title payloads (HARD CONSTRAINT #10): the structural
//! [`Manifest`](crate::manifest::Manifest) (hot path) and the
//! [`TitleInfo`] payload (cover + metadata, cached by `info_hash`). Bulk
//! transfer is plain HTTP (TDD §5); live state rides the `/ws` channel via the
//! [`ServerMsg`] / [`ClientMsg`] envelopes, which auto-reconnect + resync
//! (HARD CONSTRAINT #12 / F15).

use crate::hashing::Hash;
use crate::jukebox::OrderMode;
use serde::{Deserialize, Serialize};

/// App mode: a desktop install runs as one of these (F0.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Client,
    Playback,
}

// ───────────────────────────── Titles ──────────────────────────────

/// `GET /titles` row. `info_hash` lets a client know if its cached cover/info
/// is stale without refetching the payload (F4.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TitleSummary {
    pub id: u64,
    pub name: String,
    pub label: Option<String>,
    pub total_size: u64,
    pub file_count: u64,
    pub manifest_ver: u32,
    pub state: String,
    pub last_scan: i64,
    pub info_hash: Option<Hash>,
    pub has_cover: bool,
    pub has_install_script: bool,
}

impl TitleSummary {
    /// Display label with graceful fallback to folder name (F2.3 / F4.1).
    pub fn display(&self) -> &str {
        self.label
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.name)
    }
}

/// One launch entry from `info.json`'s `launch` block (F16.7).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchEntry {
    pub name: String,
    pub exe: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// `install_script` block — Windows only (F16.1).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallScript {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<String>,
}

/// `GET /titles/{id}/info` — the title-info payload (metadata + base64 cover).
/// Fetched on browse render, cached by `info_hash` (F4.1). All metadata optional
/// with graceful fallback.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TitleInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub players: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blurb: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Base64-encoded cover image (`data:`-less), or None → placeholder tile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cover_b64: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub launch: Vec<LaunchEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_script: Option<InstallScript>,
}

/// The `.blt/info.json` **input** format the scanner parses (TDD §3.3). All
/// fields optional; this is distinct from the served [`TitleInfo`] (which adds
/// the embedded cover and an `info_hash`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SidecarInfo {
    pub name: Option<String>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub players: Option<String>,
    pub blurb: Option<String>,
    pub link: Option<String>,
    #[serde(default)]
    pub launch: Vec<LaunchEntry>,
    pub install_script: Option<InstallScript>,
}

// ───────────────────────────── Shares ──────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareKind {
    File,
    Folder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareSummary {
    pub id: u64,
    pub name: String,
    pub kind: ShareKind,
    pub size: u64,
    pub file_count: u64,
    pub owner_name: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareFileEntry {
    pub rel_path: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareListing {
    pub share: ShareSummary,
    pub files: Vec<ShareFileEntry>,
}

// ──────────────────────────── Jukebox ──────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemType {
    Youtube,
    DirectUrl,
    SharedFile,
    External,
}

impl ItemType {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemType::Youtube => "youtube",
            ItemType::DirectUrl => "direct_url",
            ItemType::SharedFile => "shared_file",
            ItemType::External => "external",
        }
    }
    pub fn from_str_opt(s: &str) -> Option<Self> {
        Some(match s {
            "youtube" => ItemType::Youtube,
            "direct_url" => ItemType::DirectUrl,
            "shared_file" => ItemType::SharedFile,
            "external" => ItemType::External,
            _ => return None,
        })
    }
    /// External/DRM items are launched in the real browser/app (F10).
    pub fn is_external(self) -> bool {
        matches!(self, ItemType::External)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JukeboxItemDto {
    pub id: u64,
    #[serde(rename = "type")]
    pub item_type: ItemType,
    /// URL, or `share_id` for a shared file (F8.2).
    #[serde(rename = "ref")]
    pub reference: String,
    pub title: Option<String>,
    pub added_by: String,
    pub added_by_id: String,
    pub added_at: i64,
    pub votes: u32,
    /// Whether the requesting client has upvoted this item (F8.4 toggle UI).
    #[serde(default)]
    pub voted_by_me: bool,
    pub state: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackState {
    Idle,
    PlayingEmbedded,
    PlayingExternal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JukeboxState {
    pub mode: OrderMode,
    pub playback_state: PlaybackState,
    pub now_playing: Option<JukeboxItemDto>,
    pub up_next: Vec<JukeboxItemDto>,
}

// ───────────────────────────── Roster ──────────────────────────────

/// Live roster entry (F13.1). `throughput_bps` is the measured seed rate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RosterEntry {
    pub client_id: String,
    pub display_name: String,
    pub machine_name: String,
    pub activity: String,
    pub server_only: bool,
    pub throughput_bps: Option<u64>,
}

/// A peer in the registry for a `(title_id, manifest_ver)`. The downloader pulls
/// the peer's have-bitmap and chunks from `chunk_endpoint` over HTTP (keeps the
/// WS channel light).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerEntry {
    pub client_id: String,
    pub chunk_endpoint: String,
}

// ─────────────────────── WebSocket envelopes ───────────────────────

/// Client → server messages on `/ws`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Sent on connect (and after reconnect) to (re)register the session.
    Hello {
        client_id: String,
        display_name: String,
        machine_name: String,
        mode: Mode,
    },
    /// Advertise that this client seeds a title at `chunk_endpoint` (F4.10/§7).
    Announce {
        title_id: u64,
        manifest_ver: u32,
        chunk_endpoint: String,
    },
    /// Stop advertising a title (download removed / share-back turned off).
    Unannounce { title_id: u64, manifest_ver: u32 },
    /// Report current activity + measured seed throughput for the roster (F13).
    Activity {
        activity: String,
        throughput_bps: Option<u64>,
        server_only: bool,
    },
    /// Ask for the peer list for a title (F4.10).
    RequestPeers { title_id: u64, manifest_ver: u32 },
    /// Reachability self-test result for `peer` (F13.4-.5).
    ReachabilityResult { peer: String, reachable: bool },
    /// Full state resync request after a reconnect (HARD CONSTRAINT #12 / F15.1).
    Resync,
    /// Keep-alive.
    Ping,
}

/// Server → client messages on `/ws`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Sent right after connect.
    Welcome {
        server_uuid: String,
        server_label: String,
    },
    /// Authoritative jukebox snapshot (pushed on every change + on resync).
    Jukebox(JukeboxState),
    /// Live roster snapshot (F13.2).
    Roster(Vec<RosterEntry>),
    /// Peer list for a title the client asked about (F4.10).
    Peers {
        title_id: u64,
        manifest_ver: u32,
        peers: Vec<PeerEntry>,
    },
    /// 1–2 peer addresses to probe for the reachability self-test (F13.4).
    ProbePeers {
        peers: Vec<PeerEntry>,
    },
    /// A title was (re)published — clients refresh "update available" state.
    TitleChanged {
        title_id: u64,
        manifest_ver: u32,
    },
    Pong,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_envelope_roundtrips_tagged() {
        let m = ClientMsg::Hello {
            client_id: "c1".into(),
            display_name: "Rick".into(),
            machine_name: "rick-mac".into(),
            mode: Mode::Client,
        };
        let j = serde_json::to_string(&m).unwrap();
        assert!(j.contains("\"type\":\"hello\""));
        let back: ClientMsg = serde_json::from_str(&j).unwrap();
        assert!(matches!(back, ClientMsg::Hello { .. }));
    }

    #[test]
    fn server_msg_jukebox_tag() {
        let st = JukeboxState {
            mode: OrderMode::Fair,
            playback_state: PlaybackState::Idle,
            now_playing: None,
            up_next: vec![],
        };
        let j = serde_json::to_string(&ServerMsg::Jukebox(st)).unwrap();
        assert!(j.contains("\"type\":\"jukebox\""));
        assert!(j.contains("\"mode\":\"fair\""));
    }

    #[test]
    fn jukebox_item_renames_type_and_ref() {
        let it = JukeboxItemDto {
            id: 1,
            item_type: ItemType::Youtube,
            reference: "https://youtu.be/x".into(),
            title: Some("Song".into()),
            added_by: "Rick".into(),
            added_by_id: "c1".into(),
            added_at: 0,
            votes: 2,
            voted_by_me: true,
            state: "queued".into(),
        };
        let j = serde_json::to_string(&it).unwrap();
        assert!(j.contains("\"type\":\"youtube\""));
        assert!(j.contains("\"ref\":\"https://youtu.be/x\""));
    }

    #[test]
    fn sidecar_info_all_optional() {
        let s: SidecarInfo = serde_json::from_str("{}").unwrap();
        assert!(s.name.is_none());
        assert!(s.launch.is_empty());
    }

    #[test]
    fn sidecar_info_parses_full_example() {
        // The example from TDD §3.3.
        let json = r#"{ "name": "Some Game", "year": 2021, "genre": "Co-op shooter",
          "players": "1-4 local", "blurb": "Short description.",
          "launch": [
            { "name": "Play", "exe": "Game.exe", "args": "-windowed", "cwd": "." },
            { "name": "Dedicated Server", "exe": "DedicatedServer.exe" }
          ],
          "install_script": { "windows": "install.ps1" } }"#;
        let s: SidecarInfo = serde_json::from_str(json).unwrap();
        assert_eq!(s.name.as_deref(), Some("Some Game"));
        assert_eq!(s.launch.len(), 2);
        assert_eq!(s.launch[0].args.as_deref(), Some("-windowed"));
        assert_eq!(
            s.install_script.unwrap().windows.as_deref(),
            Some("install.ps1")
        );
    }

    #[test]
    fn title_summary_display_fallback() {
        let mut t = TitleSummary {
            id: 1,
            name: "folder_name".into(),
            label: None,
            total_size: 0,
            file_count: 0,
            manifest_ver: 1,
            state: "published".into(),
            last_scan: 0,
            info_hash: None,
            has_cover: false,
            has_install_script: false,
        };
        assert_eq!(t.display(), "folder_name");
        t.label = Some("Pretty Name".into());
        assert_eq!(t.display(), "Pretty Name");
        t.label = Some(String::new());
        assert_eq!(t.display(), "folder_name");
    }
}
