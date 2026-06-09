//! End-to-end integration tests: boot the real server (all three listeners on
//! ephemeral ports) and exercise the milestone acceptance criteria over HTTP/WS
//! exactly as a client would.
//!
//! Covered ACs: F2 (scan→manifest→publish, sidecar, staging), F4.5-4.9
//! (chunked download, resume bitmap, verify, version bump), F5 (quick/deep
//! validation), F6 (shares incl. sanitisation + delete rules + delete race),
//! F1.6/F11 (admin auth), F8/F10 (queue, votes, external lane) over WS.

use blt_core::bitmap::Bitmap;
use blt_core::hashing::hash_bytes;
use blt_core::manifest::Manifest;
use blt_core::protocol::{ClientMsg, ItemType, Mode, ServerMsg};
use blt_core::runtime::{Component, DataRoot};
use blt_core::transfer::{validate_deep, validate_quick, verify_and_write};
use blt_server::config::Config;
use blt_server::state::{AppState, SharedState};
use blt_server::{db, library, Supervisor};
use futures_util::{SinkExt, StreamExt};
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use tempfile::TempDir;

const TEST_CHUNK: u64 = 1024; // small chunks so multi-chunk files are cheap

struct TestServer {
    state: SharedState,
    game: SocketAddr,
    share: SocketAddr,
    admin: SocketAddr,
    _tmp: TempDir,
    library: std::path::PathBuf,
    staging: std::path::PathBuf,
}

async fn boot() -> TestServer {
    let tmp = TempDir::new().expect("tempdir");
    let root = DataRoot::resolve(Some(tmp.path().join("data"))).expect("data root");
    let dirs = root.component_dirs(Component::Server);
    dirs.ensure().expect("dirs");

    let library = tmp.path().join("library");
    let staging = tmp.path().join("staging");
    let share = tmp.path().join("share");
    for d in [&library, &staging, &share] {
        fs::create_dir_all(d).expect("mkdir");
    }

    let mut config = Config::load_or_create(&dirs.config_path).expect("config");
    config.game_distribution_bind = "127.0.0.1:0".into();
    config.shared_pool_bind = "127.0.0.1:0".into();
    config.admin_panel_bind = "127.0.0.1:0".into();
    config.library_path = Some(library.clone());
    config.staging_path = Some(staging.clone());
    config.share_path = Some(share);
    config.chunk_size = TEST_CHUNK;
    config.staging_settle_secs = 0;
    config.save(&dirs.config_path).expect("save config");

    let conn = db::open(&dirs.db_path).expect("db");
    db::set_config(&conn, "chunk_size", &TEST_CHUNK.to_string()).expect("cfg");

    let state = AppState::new(conn, config, dirs.config_path.clone(), dirs);
    let (game, share_addr, admin) = Supervisor::new(state.clone()).start().await;
    TestServer {
        state,
        game: game.expect("game listener"),
        share: share_addr.expect("share listener"),
        admin: admin.expect("admin listener"),
        _tmp: tmp,
        library,
        staging,
    }
}

/// Write a deterministic pseudo-random file.
fn write_file(path: &Path, len: usize, seed: u8) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).expect("mkdir");
    }
    let data: Vec<u8> = (0..len)
        .map(|i| ((i as u64 * 31 + seed as u64) % 251) as u8)
        .collect();
    fs::write(path, data).expect("write");
}

fn make_game(library: &Path, name: &str) {
    let dir = library.join(name);
    write_file(
        &dir.join("bin/game.exe"),
        (TEST_CHUNK * 2 + 500) as usize,
        1,
    ); // 3 chunks
    write_file(&dir.join("data/assets.pak"), (TEST_CHUNK / 2) as usize, 2); // 1 chunk
    write_file(&dir.join("readme.txt"), 64, 3);
    fs::create_dir_all(dir.join(".blt")).expect("sidecar");
    fs::write(
        dir.join(".blt/info.json"),
        r#"{ "name": "Test Game", "year": 2024, "genre": "Testing",
            "launch": [{ "name": "Play", "exe": "bin/game.exe" }],
            "install_script": { "windows": "install.ps1" } }"#,
    )
    .expect("info.json");
    fs::write(dir.join(".blt/cover.png"), b"\x89PNG fake image bytes").expect("cover");
    fs::write(dir.join(".blt/install.ps1"), "Write-Host 'configured'\n").expect("script");
}

fn scan(state: &SharedState) -> library::ScanSummary {
    library::scan_library(state).expect("scan")
}

// ───────────────────────── M1: scan & publish ──────────────────────

#[tokio::test]
async fn scan_publishes_manifest_excluding_sidecar() {
    let srv = boot().await;
    make_game(&srv.library, "Game1");
    let summary = scan(&srv.state);
    assert_eq!(summary.published, vec!["Game1"]);

    let http = reqwest::Client::new();
    let titles: Vec<serde_json::Value> = http
        .get(format!("http://{}/titles", srv.game))
        .send()
        .await
        .expect("titles")
        .json()
        .await
        .expect("json");
    assert_eq!(titles.len(), 1);
    let t = &titles[0];
    assert_eq!(t["name"], "Game1");
    assert_eq!(t["manifest_ver"], 1);
    assert_eq!(t["state"], "published");
    assert!(t["info_hash"].is_string()); // sidecar present
    assert_eq!(t["has_cover"], true);
    assert_eq!(t["has_install_script"], true);
    // file_count excludes the 3 sidecar files
    assert_eq!(t["file_count"], 3);

    let id = t["id"].as_u64().expect("id");
    let manifest: Manifest = http
        .get(format!("http://{}/titles/{id}/manifest", srv.game))
        .send()
        .await
        .expect("manifest")
        .json()
        .await
        .expect("manifest json");
    manifest.validate_structure().expect("consistent");
    assert!(manifest
        .files
        .iter()
        .all(|f| !f.rel_path.starts_with(".blt")));
    assert_eq!(manifest.files.len(), 3);
    // bin/game.exe is 3 chunks at the test chunk size
    let exe = manifest
        .files
        .iter()
        .find(|f| f.rel_path == "bin/game.exe")
        .expect("exe");
    assert_eq!(exe.chunks.len(), 3);

    // Title-info payload: metadata + base64 cover, decoupled from manifest.
    let info: serde_json::Value = http
        .get(format!("http://{}/titles/{id}/info", srv.game))
        .send()
        .await
        .expect("info")
        .json()
        .await
        .expect("info json");
    assert_eq!(info["name"], "Test Game");
    assert_eq!(info["year"], 2024);
    assert!(info["cover_b64"].as_str().expect("cover").len() > 4);
    assert_eq!(info["launch"][0]["exe"], "bin/game.exe");

    // Install script delivered separately with its hash (TDD §5.2).
    let resp = http
        .get(format!("http://{}/titles/{id}/script", srv.game))
        .send()
        .await
        .expect("script");
    assert!(resp.status().is_success());
    let hash_hdr = resp
        .headers()
        .get("x-blt-hash")
        .expect("hash header")
        .to_str()
        .expect("str")
        .to_string();
    let body = resp.bytes().await.expect("body");
    assert_eq!(hash_bytes(&body).to_hex(), hash_hdr);
}

#[tokio::test]
async fn rescan_bumps_version_sidecar_change_does_not() {
    let srv = boot().await;
    make_game(&srv.library, "Game1");
    scan(&srv.state);

    let title_id = {
        let conn = srv.state.db.lock();
        db::title_id_by_name(&conn, "Game1")
            .expect("q")
            .expect("id")
    };
    let (v1, info1) = {
        let conn = srv.state.db.lock();
        let t = db::get_title(&conn, title_id).expect("q").expect("t");
        (t.manifest_ver, t.info_hash)
    };
    assert_eq!(v1, 1);

    // Unchanged re-scan: no version bump.
    let s = scan(&srv.state);
    assert!(s.published.is_empty() && s.republished.is_empty());

    // Game-file change (different size) → version bump (F2.6, F4.9).
    write_file(
        &srv.library.join("Game1/bin/game.exe"),
        (TEST_CHUNK * 3 + 100) as usize,
        9,
    );
    let s = scan(&srv.state);
    assert_eq!(s.republished, vec!["Game1"]);
    let (v2, info2) = {
        let conn = srv.state.db.lock();
        let t = db::get_title(&conn, title_id).expect("q").expect("t");
        (t.manifest_ver, t.info_hash)
    };
    assert_eq!(v2, 2);
    assert_eq!(info1, info2, "game change must not touch info payload");

    // Sidecar-only change → info_hash changes, manifest_ver does NOT (#10).
    fs::write(
        srv.library.join("Game1/.blt/info.json"),
        r#"{ "name": "Renamed Game" }"#,
    )
    .expect("write");
    let s = scan(&srv.state);
    assert_eq!(s.info_updated, vec!["Game1"]);
    let (v3, info3) = {
        let conn = srv.state.db.lock();
        let t = db::get_title(&conn, title_id).expect("q").expect("t");
        (t.manifest_ver, t.info_hash)
    };
    assert_eq!(v3, 2, "sidecar edit must not bump manifest version");
    assert_ne!(info2, info3, "info_hash must change on sidecar edit");
}

#[tokio::test]
async fn staging_promotes_only_when_stable() {
    let srv = boot().await;
    // A folder appears in staging.
    let staged = srv.staging.join("NewGame");
    write_file(&staged.join("game.bin"), 2048, 7);

    let mut tracker = library::StagingTracker::default();
    // First tick observes it (settle clock starts) — not promoted yet.
    let promoted = tracker.tick(&srv.state);
    assert!(promoted.is_empty(), "first observation must not promote");
    assert!(staged.exists());

    // The folder changes (still copying) → clock resets, still not promoted.
    write_file(&staged.join("game2.bin"), 4096, 8);
    let promoted = tracker.tick(&srv.state);
    assert!(promoted.is_empty(), "changed folder must not promote");

    // Stable now (settle=0 in tests): next tick observes, the one after promotes.
    let promoted = tracker.tick(&srv.state);
    assert_eq!(promoted, vec!["NewGame"], "stable folder promotes");
    assert!(!staged.exists(), "staging folder moved");
    assert!(
        srv.library.join("NewGame/game.bin").exists(),
        "atomic rename into library"
    );

    // And it scans/publishes.
    let s = scan(&srv.state);
    assert_eq!(s.published, vec!["NewGame"]);
}

// ──────────────── M2: chunked download, resume, validate ───────────

/// Download a title exactly like the client will: manifest → chunk loop with
/// verify-before-write + bitmap, interrupted halfway, resumed, validated.
#[tokio::test]
async fn chunked_download_resume_and_validate() {
    let srv = boot().await;
    make_game(&srv.library, "Game1");
    scan(&srv.state);
    let http = reqwest::Client::new();

    let titles: Vec<serde_json::Value> = http
        .get(format!("http://{}/titles", srv.game))
        .send()
        .await
        .expect("titles")
        .json()
        .await
        .expect("json");
    let id = titles[0]["id"].as_u64().expect("id");
    let manifest: Manifest = http
        .get(format!("http://{}/titles/{id}/manifest", srv.game))
        .send()
        .await
        .expect("manifest")
        .json()
        .await
        .expect("json");

    let dest = TempDir::new().expect("dest");
    let locators = manifest.chunk_locators();
    let mut bitmap = Bitmap::new(manifest.chunk_count());

    // Phase 1: download the first half, then "lose Wi-Fi".
    let half = locators.len() / 2;
    for loc in &locators[..half] {
        let bytes = http
            .get(format!(
                "http://{}/chunks/{}/{}",
                srv.game, loc.file_id, loc.chunk_idx
            ))
            .send()
            .await
            .expect("chunk")
            .bytes()
            .await
            .expect("bytes");
        // HARD CONSTRAINT #1: verify before write.
        verify_and_write(dest.path(), &loc.rel_path, loc.offset, &bytes, &loc.hash)
            .expect("verified chunk");
        bitmap.set(loc.global_idx);
    }
    // Quick validation now reports the incomplete state.
    let report = validate_quick(&manifest, dest.path());
    assert!(!report.all_ok(), "half-downloaded title must not validate");

    // Phase 2: resume — fetch ONLY missing bits (the bitmap drives it).
    let missing: Vec<u64> = bitmap.missing().collect();
    assert_eq!(missing.len(), locators.len() - half);
    for gi in missing {
        let loc = &locators[gi as usize];
        let bytes = http
            .get(format!(
                "http://{}/chunks/{}/{}",
                srv.game, loc.file_id, loc.chunk_idx
            ))
            .send()
            .await
            .expect("chunk")
            .bytes()
            .await
            .expect("bytes");
        verify_and_write(dest.path(), &loc.rel_path, loc.offset, &bytes, &loc.hash)
            .expect("verified chunk");
        bitmap.set(loc.global_idx);
    }
    assert!(bitmap.is_complete());

    // Quick + deep validation both pass (F5.1, F5.2).
    assert!(validate_quick(&manifest, dest.path()).all_ok());
    assert!(validate_deep(&manifest, dest.path()).all_ok());

    // A tampered chunk is rejected and never written (F4.8/F4.13).
    let loc = &locators[0];
    let mut bytes = http
        .get(format!(
            "http://{}/chunks/{}/{}",
            srv.game, loc.file_id, loc.chunk_idx
        ))
        .send()
        .await
        .expect("chunk")
        .bytes()
        .await
        .expect("bytes")
        .to_vec();
    bytes[0] ^= 0xFF; // bad peer poisons the chunk
    let before = fs::read(dest.path().join(&loc.rel_path)).expect("read");
    assert!(verify_and_write(dest.path(), &loc.rel_path, loc.offset, &bytes, &loc.hash).is_err());
    let after = fs::read(dest.path().join(&loc.rel_path)).expect("read");
    assert_eq!(before, after, "rejected chunk must not touch the file");
}

#[tokio::test]
async fn http_range_requests_work() {
    let srv = boot().await;
    make_game(&srv.library, "Game1");
    scan(&srv.state);
    let http = reqwest::Client::new();

    let titles: Vec<serde_json::Value> = http
        .get(format!("http://{}/titles", srv.game))
        .send()
        .await
        .expect("titles")
        .json()
        .await
        .expect("json");
    let id = titles[0]["id"].as_u64().expect("id");
    let manifest: Manifest = http
        .get(format!("http://{}/titles/{id}/manifest", srv.game))
        .send()
        .await
        .expect("manifest")
        .json()
        .await
        .expect("json");
    let exe = manifest
        .files
        .iter()
        .find(|f| f.rel_path == "bin/game.exe")
        .expect("exe");

    // Range fetch of the second chunk's span matches the chunk endpoint bytes.
    let c1 = &exe.chunks[1];
    let ranged = http
        .get(format!("http://{}/titles/{id}/files/{}", srv.game, exe.id))
        .header(
            "Range",
            format!("bytes={}-{}", c1.offset, c1.offset + c1.size - 1),
        )
        .send()
        .await
        .expect("range");
    assert_eq!(ranged.status().as_u16(), 206);
    let ranged_bytes = ranged.bytes().await.expect("bytes");
    assert_eq!(hash_bytes(&ranged_bytes), c1.hash);
}

// ───────────────────────── M5: shared pool ─────────────────────────

#[tokio::test]
async fn share_upload_browse_download_delete_rules() {
    let srv = boot().await;
    let http = reqwest::Client::new();
    let base = format!("http://{}", srv.share);

    // Upload a folder share with a structure and a Windows-hostile name.
    let form = reqwest::multipart::Form::new()
        .part(
            "meta",
            reqwest::multipart::Part::text(
                r#"{"name":"My Mod Pack","kind":"folder","owner_name":"Rick"}"#,
            ),
        )
        .part(
            "file",
            reqwest::multipart::Part::bytes(vec![1u8; 100]).file_name("readme.txt"),
        )
        .part(
            "file",
            reqwest::multipart::Part::bytes(vec![2u8; 200]).file_name("maps/dust2.bsp"),
        )
        .part(
            "file",
            // ':' is illegal on Windows → must be sanitised on write (#11)
            reqwest::multipart::Part::bytes(vec![3u8; 50]).file_name("notes:final.txt"),
        );
    let created: serde_json::Value = http
        .post(format!("{base}/shares"))
        .header("x-blt-client-id", "client-A")
        .multipart(form)
        .send()
        .await
        .expect("upload")
        .json()
        .await
        .expect("json");
    let share_id = created["id"].as_u64().expect("id");
    assert_eq!(created["kind"], "folder");
    assert_eq!(created["file_count"], 3);
    assert_eq!(created["owner_name"], "Rick");

    // Browse listing (F6.5) — sanitised name recorded.
    let listing: serde_json::Value = http
        .get(format!("{base}/shares/{share_id}"))
        .send()
        .await
        .expect("listing")
        .json()
        .await
        .expect("json");
    let rels: Vec<&str> = listing["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["rel_path"].as_str().expect("rel"))
        .collect();
    assert!(rels.contains(&"maps/dust2.bsp"));
    assert!(
        rels.contains(&"notes_final.txt"),
        "illegal char sanitised: {rels:?}"
    );

    // Download one file with a Range (also the playback streaming path, F9.2).
    let resp = http
        .get(format!("{base}/shares/{share_id}/files/maps/dust2.bsp"))
        .header("Range", "bytes=0-99")
        .send()
        .await
        .expect("file");
    assert_eq!(resp.status().as_u16(), 206);
    assert_eq!(resp.bytes().await.expect("bytes").len(), 100);

    // Delete by a non-owner is forbidden (F6.9).
    let resp = http
        .delete(format!("{base}/shares/{share_id}"))
        .header("x-blt-client-id", "client-B")
        .send()
        .await
        .expect("del");
    assert_eq!(resp.status().as_u16(), 403);

    // Delete by the owner works.
    let resp = http
        .delete(format!("{base}/shares/{share_id}"))
        .header("x-blt-client-id", "client-A")
        .send()
        .await
        .expect("del");
    assert_eq!(resp.status().as_u16(), 204);

    // Delete race (F15.4): fetching a deleted share's file errors gracefully.
    let resp = http
        .get(format!("{base}/shares/{share_id}/files/maps/dust2.bsp"))
        .send()
        .await
        .expect("get");
    assert_eq!(resp.status().as_u16(), 410);
}

#[tokio::test]
async fn share_upload_rejects_traversal() {
    let srv = boot().await;
    let http = reqwest::Client::new();
    let form = reqwest::multipart::Form::new()
        .part(
            "meta",
            reqwest::multipart::Part::text(r#"{"name":"evil","kind":"folder","owner_name":"X"}"#),
        )
        .part(
            "file",
            reqwest::multipart::Part::bytes(vec![1u8; 10]).file_name("../../escape.sh"),
        );
    let resp = http
        .post(format!("http://{}/shares", srv.share))
        .multipart(form)
        .send()
        .await
        .expect("upload");
    assert_eq!(resp.status().as_u16(), 400, "traversal must be rejected");
}

// ──────────────────────── M8: admin auth gate ──────────────────────

#[tokio::test]
async fn admin_auth_setup_login_gate() {
    let srv = boot().await;
    let http = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .expect("client");
    let base = format!("http://{}", srv.admin);

    // Gated route without a session → 401.
    let r = http
        .get(format!("{base}/api/titles"))
        .send()
        .await
        .expect("get");
    assert_eq!(r.status().as_u16(), 401);

    // auth-state reports setup needed.
    let st: serde_json::Value = http
        .get(format!("{base}/api/auth-state"))
        .send()
        .await
        .expect("get")
        .json()
        .await
        .expect("json");
    assert_eq!(st["needs_setup"], true);

    // First-run setup issues a session cookie (F1.6).
    let r = http
        .post(format!("{base}/api/setup"))
        .json(&serde_json::json!({"password": "hunter2"}))
        .send()
        .await
        .expect("setup");
    assert!(r.status().is_success());

    // Setup can't run twice.
    let r = http
        .post(format!("{base}/api/setup"))
        .json(&serde_json::json!({"password": "again"}))
        .send()
        .await
        .expect("setup2");
    assert_eq!(r.status().as_u16(), 409);

    // Gated route now works with the cookie.
    let r = http
        .get(format!("{base}/api/titles"))
        .send()
        .await
        .expect("titles");
    assert!(r.status().is_success());

    // Fresh client: wrong password rejected, right password accepted.
    let http2 = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .expect("client2");
    let r = http2
        .post(format!("{base}/api/login"))
        .json(&serde_json::json!({"password": "wrong"}))
        .send()
        .await
        .expect("login");
    assert_eq!(r.status().as_u16(), 401);
    let r = http2
        .post(format!("{base}/api/login"))
        .json(&serde_json::json!({"password": "hunter2"}))
        .send()
        .await
        .expect("login");
    assert!(r.status().is_success());
}

// ─────────────────── M6/M7: jukebox over WS + admin ────────────────

async fn ws_connect(
    addr: SocketAddr,
    client_id: &str,
    name: &str,
    mode: Mode,
) -> (
    impl SinkExt<
            tokio_tungstenite::tungstenite::Message,
            Error = tokio_tungstenite::tungstenite::Error,
        > + Unpin,
    impl StreamExt<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
) {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("ws connect");
    let (mut sink, stream) = ws.split();
    let hello = serde_json::to_string(&ClientMsg::Hello {
        client_id: client_id.into(),
        display_name: name.into(),
        machine_name: "test-machine".into(),
        mode,
    })
    .expect("ser");
    sink.send(tokio_tungstenite::tungstenite::Message::Text(hello))
        .await
        .expect("send hello");
    (sink, stream)
}

/// Discard everything already buffered on the stream (returns on 50ms of quiet).
async fn drain_pending<S>(stream: &mut S)
where
    S: StreamExt<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    while tokio::time::timeout(std::time::Duration::from_millis(50), stream.next())
        .await
        .is_ok()
    {}
}

/// Read server messages until one matches `pred` (with a timeout).
async fn next_matching<S>(stream: &mut S, mut pred: impl FnMut(&ServerMsg) -> bool) -> ServerMsg
where
    S: StreamExt<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(Ok(msg)) = stream.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                if let Ok(parsed) = serde_json::from_str::<ServerMsg>(&t) {
                    if pred(&parsed) {
                        return parsed;
                    }
                }
            }
        }
        panic!("ws stream ended");
    })
    .await
    .expect("timeout waiting for ws message")
}

#[tokio::test]
async fn jukebox_ws_add_vote_and_external_lane() {
    let srv = boot().await;

    // A player client and the playback machine connect.
    let (mut player_tx, mut player_rx) =
        ws_connect(srv.game, "player-1", "Rick", Mode::Client).await;
    let _ = next_matching(&mut player_rx, |m| matches!(m, ServerMsg::Welcome { .. })).await;

    let (mut playback_tx, mut playback_rx) =
        ws_connect(srv.game, "playback-1", "TV", Mode::Playback).await;
    let _ = next_matching(&mut playback_rx, |m| matches!(m, ServerMsg::Welcome { .. })).await;

    // Player adds a YouTube item and an external (Netflix) item (F8.1).
    for (ty, reference) in [
        (ItemType::Youtube, "https://youtu.be/dQw4w9WgXcQ"),
        (ItemType::External, "https://netflix.com/title/1"),
    ] {
        let add = serde_json::to_string(&ClientMsg::JukeboxAdd {
            item_type: ty,
            reference: reference.into(),
            title: Some(format!("{ty:?}")),
        })
        .expect("ser");
        player_tx
            .send(tokio_tungstenite::tungstenite::Message::Text(add))
            .await
            .expect("send");
    }
    // Player sees the queue grow to 2.
    let state = next_matching(
        &mut player_rx,
        |m| matches!(m, ServerMsg::Jukebox(s) if s.up_next.len() == 2),
    )
    .await;
    let ServerMsg::Jukebox(snapshot) = state else {
        unreachable!()
    };
    assert_eq!(snapshot.up_next[0].added_by, "Rick");
    let yt_id = snapshot
        .up_next
        .iter()
        .find(|i| i.item_type == ItemType::Youtube)
        .expect("yt")
        .id;

    // Vote keyed on client_id; voted_by_me reflected per viewer (F8.4).
    let vote = serde_json::to_string(&ClientMsg::JukeboxVote { item_id: yt_id }).expect("ser");
    player_tx
        .send(tokio_tungstenite::tungstenite::Message::Text(vote))
        .await
        .expect("send");
    let state = next_matching(&mut player_rx, |m| {
        matches!(m, ServerMsg::Jukebox(s)
            if s.up_next.iter().any(|i| i.id == yt_id && i.votes == 1))
    })
    .await;
    let ServerMsg::Jukebox(snapshot) = state else {
        unreachable!()
    };
    assert!(
        snapshot
            .up_next
            .iter()
            .find(|i| i.id == yt_id)
            .expect("yt")
            .voted_by_me
    );

    // Playback presses Next: the voted YouTube item starts (embedded).
    let next = serde_json::to_string(&ClientMsg::JukeboxNext).expect("ser");
    playback_tx
        .send(tokio_tungstenite::tungstenite::Message::Text(next.clone()))
        .await
        .expect("send");
    let state = next_matching(
        &mut playback_rx,
        |m| matches!(m, ServerMsg::Jukebox(s) if s.now_playing.is_some()),
    )
    .await;
    let ServerMsg::Jukebox(snapshot) = state else {
        unreachable!()
    };
    assert_eq!(snapshot.now_playing.as_ref().expect("np").id, yt_id);
    assert_eq!(
        snapshot.playback_state,
        blt_core::protocol::PlaybackState::PlayingEmbedded
    );

    // Embedded item ends → auto-advance into the external item, which awaits a
    // human (F9.3, F10.2).
    let ended = serde_json::to_string(&ClientMsg::JukeboxEnded).expect("ser");
    playback_tx
        .send(tokio_tungstenite::tungstenite::Message::Text(ended))
        .await
        .expect("send");
    let state = next_matching(&mut playback_rx, |m| {
        matches!(m, ServerMsg::Jukebox(s)
            if s.playback_state == blt_core::protocol::PlaybackState::PlayingExternal)
    })
    .await;
    let ServerMsg::Jukebox(snapshot) = state else {
        unreachable!()
    };
    assert_eq!(
        snapshot.now_playing.expect("np").item_type,
        ItemType::External
    );

    // A *client* sending Next is ignored (only playback/admin advance, F8.7).
    player_tx
        .send(tokio_tungstenite::tungstenite::Message::Text(next))
        .await
        .expect("send");
    // Drain any buffered broadcasts (older snapshots queue on the stream),
    // then resync and assert on the fresh snapshot.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    drain_pending(&mut player_rx).await;
    let resync = serde_json::to_string(&ClientMsg::Resync).expect("ser");
    player_tx
        .send(tokio_tungstenite::tungstenite::Message::Text(resync))
        .await
        .expect("send");
    let state = next_matching(&mut player_rx, |m| matches!(m, ServerMsg::Jukebox(_))).await;
    let ServerMsg::Jukebox(snapshot) = state else {
        unreachable!()
    };
    assert_eq!(
        snapshot.playback_state,
        blt_core::protocol::PlaybackState::PlayingExternal,
        "client Next must be ignored"
    );

    // Playback presses Next out of the external item → queue empty → Idle (F10.4).
    let next = serde_json::to_string(&ClientMsg::JukeboxNext).expect("ser");
    playback_tx
        .send(tokio_tungstenite::tungstenite::Message::Text(next))
        .await
        .expect("send");
    let state = next_matching(&mut playback_rx, |m| {
        matches!(m, ServerMsg::Jukebox(s)
            if s.playback_state == blt_core::protocol::PlaybackState::Idle)
    })
    .await;
    let ServerMsg::Jukebox(snapshot) = state else {
        unreachable!()
    };
    assert!(snapshot.now_playing.is_none());
}

#[tokio::test]
async fn roster_reflects_joins_and_leaves() {
    let srv = boot().await;
    let (_tx1, mut rx1) = ws_connect(srv.game, "c1", "Alice", Mode::Client).await;
    let _ = next_matching(&mut rx1, |m| matches!(m, ServerMsg::Welcome { .. })).await;

    // Second client joins → roster broadcast includes both.
    let (tx2, mut rx2) = ws_connect(srv.game, "c2", "Bob", Mode::Client).await;
    let _ = next_matching(&mut rx2, |m| matches!(m, ServerMsg::Welcome { .. })).await;
    let roster = next_matching(
        &mut rx1,
        |m| matches!(m, ServerMsg::Roster { roster } if roster.len() == 2),
    )
    .await;
    let ServerMsg::Roster { roster: entries } = roster else {
        unreachable!()
    };
    let names: Vec<&str> = entries.iter().map(|e| e.display_name.as_str()).collect();
    assert_eq!(names, vec!["Alice", "Bob"]);

    // Bob leaves → roster shrinks (F15.3).
    drop(tx2);
    drop(rx2);
    let roster = next_matching(
        &mut rx1,
        |m| matches!(m, ServerMsg::Roster { roster } if roster.len() == 1),
    )
    .await;
    let ServerMsg::Roster { roster: entries } = roster else {
        unreachable!()
    };
    assert_eq!(entries[0].display_name, "Alice");
}
