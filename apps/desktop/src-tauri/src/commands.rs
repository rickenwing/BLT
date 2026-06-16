//! Tauri commands — the IPC contract with the React UI. Confirmation gates
//! (uploads, deletes, download-all, external launches — HARD CONSTRAINT #5)
//! live in the UI; commands assume the confirmed intent but enforce the
//! non-negotiables (verify-before-write, sanitisation, mode rules) themselves.

use crate::state::{Settings, Shared};
use crate::{db, downloads, freespace, scripts, server_api};
use blt_core::pathsafe::{resolve_collision, sanitize_rel_path};
use blt_core::protocol::{ClientMsg, ItemType, Mode, TitleInfo, TitleSummary};
use blt_core::transfer::{repair_plan, validate_deep, validate_quick};
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;
use tauri::State;
use tracing::info;

type Cmd<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

// ───────────────────────── app state / settings ─────────────────────

#[derive(Serialize)]
pub struct AppBootState {
    pub settings: Settings,
    pub connection: crate::state::Connection_,
    pub mode_chosen: bool,
    pub version: String,
}

#[tauri::command]
pub fn get_app_state(state: State<'_, Shared>) -> AppBootState {
    let mode_chosen = {
        let conn = state.db.lock();
        db::get_setting(&conn, "mode_chosen")
            .ok()
            .flatten()
            .is_some()
    };
    AppBootState {
        settings: state.settings.read().clone(),
        connection: state.connection.read().clone(),
        mode_chosen,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// First-run mode pick (F0.5) / later mode change (restarts into the new mode).
#[tauri::command]
pub fn choose_mode(state: State<'_, Shared>, app: tauri::AppHandle, mode: String) -> Cmd<()> {
    let mode = match mode.as_str() {
        "playback" => Mode::Playback,
        _ => Mode::Client,
    };
    {
        let mut s = state.settings.write();
        s.mode = mode;
        let conn = state.db.lock();
        s.persist(&conn).map_err(err)?;
        db::set_setting(&conn, "mode_chosen", "true").map_err(err)?;
    }
    // Mode switch restarts the app (F14.1 analogue for plain mode change).
    app.restart();
}

#[derive(serde::Deserialize)]
pub struct SettingsPatch {
    pub display_name: Option<String>,
    pub default_download_root: Option<String>,
    pub upload_cap_bytes_per_sec: Option<u64>,
    pub share_back: Option<bool>,
}

#[tauri::command]
pub fn update_settings(state: State<'_, Shared>, patch: SettingsPatch) -> Cmd<Settings> {
    let mut renamed = false;
    {
        let mut s = state.settings.write();
        if let Some(n) = patch.display_name {
            if !n.trim().is_empty() && n != s.display_name {
                s.display_name = n.trim().to_string();
                renamed = true;
            }
        }
        if let Some(r) = patch.default_download_root {
            s.default_download_root = (!r.is_empty()).then_some(r);
        }
        if let Some(c) = patch.upload_cap_bytes_per_sec {
            s.upload_cap_bytes_per_sec = c.max(64 * 1024);
        }
        if let Some(b) = patch.share_back {
            s.share_back = b;
        }
        let conn = state.db.lock();
        s.persist(&conn).map_err(err)?;
    }
    if renamed {
        // Re-hello so shares/jukebox attribution follows the new name (F7.2).
        let s = state.settings.read().clone();
        let machine = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_default();
        state.send_ws(ClientMsg::Hello {
            client_id: s.client_id,
            display_name: s.display_name,
            machine_name: machine,
            mode: s.mode,
        });
    }
    Ok(state.settings.read().clone())
}

// ───────────────────────── servers / connection ─────────────────────

#[tauri::command]
pub fn list_servers(state: State<'_, Shared>) -> Cmd<Vec<db::ServerRow>> {
    let conn = state.db.lock();
    db::list_servers(&conn).map_err(err)
}

/// Connect (manual entry or a discovered row). The WS loop picks the endpoint
/// up on its next attempt; per-service endpoints route automatically (F3.3).
#[tauri::command]
pub fn connect_to(
    state: State<'_, Shared>,
    app: tauri::AppHandle,
    game_endpoint: String,
    share_endpoint: Option<String>,
    label: Option<String>,
) -> Cmd<()> {
    if !blt_core::discovery::is_endpoint(&game_endpoint) {
        return Err("expected host:port".into());
    }
    {
        let mut c = state.connection.write();
        c.game_endpoint = Some(game_endpoint.clone());
        c.share_endpoint = share_endpoint.clone();
        c.server_label = label.clone();
        c.ws_connected = false;
    }
    {
        let conn = state.db.lock();
        let _ = db::upsert_server(
            &conn,
            None,
            label.as_deref().unwrap_or(&game_endpoint),
            &game_endpoint,
            share_endpoint.as_deref().unwrap_or(""),
            crate::now_unix(),
        );
        let mut s = state.settings.write();
        s.last_server = Some(game_endpoint);
        let _ = s.persist(&conn);
    }
    let _ = tauri::Emitter::emit(&app, "connection-changed", ());
    Ok(())
}

#[tauri::command]
pub fn connection_state(state: State<'_, Shared>) -> crate::state::Connection_ {
    state.connection.read().clone()
}

// ───────────────────────────── library ──────────────────────────────

#[derive(Serialize)]
pub struct TitleWithLocal {
    #[serde(flatten)]
    pub title: TitleSummary,
    /// not_downloaded | partial | complete | update_available
    pub local_state: String,
    pub local_dest: Option<String>,
    pub local_ver: Option<u32>,
}

#[tauri::command]
pub async fn fetch_titles(state: State<'_, Shared>) -> Cmd<Vec<TitleWithLocal>> {
    let game = state
        .connection
        .read()
        .game_endpoint
        .clone()
        .ok_or("not connected")?;
    let titles = server_api::list_titles(&game).await.map_err(err)?;
    let rows = {
        let conn = state.db.lock();
        db::list_downloads(&conn).map_err(err)?
    };
    Ok(titles
        .into_iter()
        .map(|t| {
            let mine: Vec<_> = rows.iter().filter(|r| r.title_id == t.id).collect();
            let complete = mine
                .iter()
                .filter(|r| r.status == "complete")
                .max_by_key(|r| r.manifest_ver);
            let partial = mine
                .iter()
                .filter(|r| r.status != "complete")
                .max_by_key(|r| r.manifest_ver);
            // A recorded download whose folder no longer exists on disk (the
            // user deleted it, or the volume is gone) reads as not_downloaded,
            // so the Library reflects reality without a manual validate.
            let present = |d: &str| std::path::Path::new(d).exists();
            let (local_state, local_dest, local_ver) = match (complete, partial) {
                (Some(c), _) if c.manifest_ver == t.manifest_ver && present(&c.dest_path) => {
                    ("complete", Some(c.dest_path.clone()), Some(c.manifest_ver))
                }
                (Some(c), _) if present(&c.dest_path) => (
                    // finished on an older version → flag update (F4.9)
                    "update_available",
                    Some(c.dest_path.clone()),
                    Some(c.manifest_ver),
                ),
                (None, Some(p)) if present(&p.dest_path) => {
                    ("partial", Some(p.dest_path.clone()), Some(p.manifest_ver))
                }
                _ => ("not_downloaded", None, None),
            };
            TitleWithLocal {
                title: t,
                local_state: local_state.to_string(),
                local_dest,
                local_ver,
            }
        })
        .collect())
}

/// Title-info payload, cached by `info_hash` (F4.1): refetched only when the
/// hash changes; cached covers survive offline browsing.
#[tauri::command]
pub async fn fetch_title_info(
    state: State<'_, Shared>,
    title_id: u64,
    info_hash: Option<String>,
) -> Cmd<TitleInfo> {
    let cache = state.dirs.cache.clone();
    if let Some(h) = &info_hash {
        let cached = cache.join(format!("info-{title_id}-{h}.json"));
        if let Ok(text) = tokio::fs::read_to_string(&cached).await {
            if let Ok(info) = serde_json::from_str::<TitleInfo>(&text) {
                return Ok(info);
            }
        }
    }
    let game = state
        .connection
        .read()
        .game_endpoint
        .clone()
        .ok_or("not connected")?;
    let info = server_api::title_info(&game, title_id).await.map_err(err)?;
    if let Some(h) = &info_hash {
        // Cache + drop stale entries for this title.
        let _ = tokio::fs::create_dir_all(&cache).await;
        if let Ok(mut rd) = tokio::fs::read_dir(&cache).await {
            while let Ok(Some(e)) = rd.next_entry().await {
                if let Some(name) = e.file_name().to_str() {
                    if name.starts_with(&format!("info-{title_id}-")) {
                        let _ = tokio::fs::remove_file(e.path()).await;
                    }
                }
            }
        }
        let _ = tokio::fs::write(
            cache.join(format!("info-{title_id}-{h}.json")),
            serde_json::to_string(&info).unwrap_or_default(),
        )
        .await;
    }
    Ok(info)
}

#[derive(Serialize)]
pub struct DownloadPlan {
    pub dest: String,
    pub needed_bytes: u64,
    pub available_bytes: Option<u64>,
    pub enough_space: bool,
}

/// Resolve destination + free-space pre-flight (F4.4). The UI warns + asks for
/// confirmation when `enough_space` is false (never hard-blocks, #6).
#[tauri::command]
pub fn prepare_download(
    state: State<'_, Shared>,
    title_id: u64,
    title_name: String,
    total_size: u64,
    dest_override: Option<String>,
) -> Cmd<DownloadPlan> {
    if state.settings.read().mode == Mode::Playback {
        return Err("playback clients never download games".into()); // HC #8
    }
    let dest = match dest_override {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => {
            let conn = state.db.lock();
            match db::title_location(&conn, title_id).map_err(err)? {
                Some(d) => PathBuf::from(d),
                None => {
                    let root = state
                        .settings
                        .read()
                        .default_download_root
                        .clone()
                        .ok_or("set a download folder first (Settings)")?;
                    let safe_name = sanitize_rel_path(&title_name)
                        .unwrap_or_else(|_| format!("title-{title_id}"));
                    PathBuf::from(root).join(safe_name)
                }
            }
        }
    };
    let (enough, available) = freespace::preflight(&dest, total_size);
    Ok(DownloadPlan {
        dest: dest.to_string_lossy().into_owned(),
        needed_bytes: total_size,
        available_bytes: available,
        enough_space: enough,
    })
}

/// Enqueue (the UI has completed any confirm step). Sequential visible queue.
#[tauri::command]
pub fn begin_download(
    state: State<'_, Shared>,
    app: tauri::AppHandle,
    title_id: u64,
    manifest_ver: u32,
    title_name: String,
    dest: String,
) -> Cmd<()> {
    if state.settings.read().mode == Mode::Playback {
        return Err("playback clients never download games".into()); // HC #8
    }
    {
        let conn = state.db.lock();
        // If the destination folder is gone (deleted game, or a fresh download),
        // drop any stale resume state so we don't treat a vanished game as
        // already complete — otherwise it'd "finish" instantly then fail to
        // validate.
        if !PathBuf::from(&dest).exists() {
            let _ = db::delete_downloads_for_title(&conn, title_id);
        }
        db::set_title_location(&conn, title_id, &dest).map_err(err)?;
    }
    state.downloads.enqueue(
        state.inner(),
        &app,
        title_id,
        manifest_ver,
        title_name,
        PathBuf::from(dest),
    );
    Ok(())
}

/// Delete a downloaded game: remove its files from disk and forget its local
/// state. UI-confirmed (HARD CONSTRAINT #5). Refuses to remove the download root
/// itself as a guard against a misconfigured destination.
#[tauri::command]
pub fn delete_game(state: State<'_, Shared>, app: tauri::AppHandle, title_id: u64) -> Cmd<()> {
    state.downloads.cancel(title_id); // stop any in-flight download first
    let dest = {
        let conn = state.db.lock();
        db::title_location(&conn, title_id).map_err(err)?
    };
    if let Some(dest) = dest {
        let p = PathBuf::from(&dest);
        if let Some(root) = state.settings.read().default_download_root.clone() {
            if p.as_path() == std::path::Path::new(&root) {
                return Err(
                    "refusing to delete the download root — remove the game folder manually".into(),
                );
            }
        }
        if p.exists() {
            std::fs::remove_dir_all(&p).map_err(err)?;
        }
    }
    {
        let conn = state.db.lock();
        db::delete_downloads_for_title(&conn, title_id).map_err(err)?;
        db::delete_title_location(&conn, title_id).map_err(err)?;
    }
    let _ = tauri::Emitter::emit(&app, "titles-changed", ());
    Ok(())
}

#[tauri::command]
pub fn pause_download(state: State<'_, Shared>, title_id: u64) {
    state.downloads.pause(title_id);
}

#[tauri::command]
pub fn resume_download(
    state: State<'_, Shared>,
    app: tauri::AppHandle,
    title_id: u64,
    manifest_ver: u32,
    title_name: String,
) -> Cmd<()> {
    state
        .downloads
        .resume(state.inner(), &app, title_id, manifest_ver, title_name)
}

#[tauri::command]
pub fn cancel_download(state: State<'_, Shared>, title_id: u64) {
    state.downloads.cancel(title_id);
}

#[tauri::command]
pub fn downloads_snapshot(state: State<'_, Shared>) -> Vec<downloads::QueueEntry> {
    state.downloads.snapshot(state.inner())
}

/// Loopback port of the YouTube embed proxy (Error 153 workaround). The
/// playback UI loads `http://127.0.0.1:<port>/yt?v=<id>` in an iframe so the
/// player gets a valid http origin.
#[tauri::command]
pub fn media_proxy_port(state: State<'_, Shared>) -> Option<u16> {
    *state.media_port.read()
}

/// Unified active-transfer list for the sidebar + title-bar indicator: in-flight
/// game downloads plus shared-pool uploads/downloads, in one uniform shape.
#[tauri::command]
pub fn active_transfers(state: State<'_, Shared>) -> Vec<crate::state::TransferRow> {
    let mut out: Vec<crate::state::TransferRow> = Vec::new();
    for q in state.downloads.snapshot(state.inner()) {
        if q.status == "active" || q.status == "pausing" {
            out.push(crate::state::TransferRow {
                id: format!("game-{}", q.title_id),
                kind: "game-download".to_string(),
                label: if q.name.is_empty() {
                    format!("Title #{}", q.title_id)
                } else {
                    q.name.clone()
                },
                done: q.bytes_done,
                total: q.bytes_total,
                speed_bps: q.speed_bps,
            });
        }
    }
    out.extend(state.share_transfers());
    out
}

/// Cancel an in-flight transfer by its sidebar row id (`share-<id>` for a
/// shared-pool upload/download, `game-<title_id>` for a game download).
#[tauri::command]
pub fn cancel_transfer(state: State<'_, Shared>, id: String) {
    if let Some(rest) = id.strip_prefix("share-") {
        if let Ok(n) = rest.parse::<u64>() {
            state.xfer_cancel(n);
        }
    } else if let Some(rest) = id.strip_prefix("game-") {
        if let Ok(n) = rest.parse::<u64>() {
            state.downloads.cancel(n);
        }
    }
}

#[derive(Serialize)]
pub struct ValidationOut {
    pub all_ok: bool,
    pub ok_count: usize,
    pub total: usize,
    pub failures: Vec<(String, String)>,
}

/// Quick (F5.1) or deep (F5.2) validation; runs off the UI thread.
#[tauri::command]
pub async fn validate_title(
    state: State<'_, Shared>,
    title_id: u64,
    deep: bool,
) -> Cmd<ValidationOut> {
    let game = state
        .connection
        .read()
        .game_endpoint
        .clone()
        .ok_or("not connected")?;
    let manifest = server_api::title_manifest(&game, title_id)
        .await
        .map_err(err)?;
    let dest = {
        let conn = state.db.lock();
        db::load_bitmap(&conn, title_id, manifest.manifest_ver)
            .map_err(err)?
            .map(|(_, _, d)| d)
            .or(db::title_location(&conn, title_id).map_err(err)?)
            .ok_or("no local copy known for this title")?
    };
    let report = tokio::task::spawn_blocking(move || {
        let root = PathBuf::from(dest);
        if deep {
            validate_deep(&manifest, &root)
        } else {
            validate_quick(&manifest, &root)
        }
    })
    .await
    .map_err(err)?;
    Ok(ValidationOut {
        all_ok: report.all_ok(),
        ok_count: report.ok_count(),
        total: report.files.len(),
        failures: report
            .failures()
            .map(|f| (f.rel_path.clone(), format!("{:?}", f.detail)))
            .collect(),
    })
}

/// Repair (F5.3): recompute bad chunks, clear their bits, re-enqueue.
#[tauri::command]
pub async fn repair_title(
    state: State<'_, Shared>,
    app: tauri::AppHandle,
    title_id: u64,
    title_name: String,
) -> Cmd<u64> {
    let game = state
        .connection
        .read()
        .game_endpoint
        .clone()
        .ok_or("not connected")?;
    let manifest = server_api::title_manifest(&game, title_id)
        .await
        .map_err(err)?;
    let ver = manifest.manifest_ver;
    let dest = {
        let conn = state.db.lock();
        db::load_bitmap(&conn, title_id, ver)
            .map_err(err)?
            .map(|(_, _, d)| d)
            .ok_or("no local copy for the current version")?
    };
    let dest_path = PathBuf::from(&dest);
    let (bad, total) = {
        let m = manifest.clone();
        let d = dest_path.clone();
        tokio::task::spawn_blocking(move || {
            let bad = repair_plan(&m, &d);
            (bad, m.chunk_count())
        })
        .await
        .map_err(err)?
    };
    let n = bad.len() as u64;
    {
        let conn = state.db.lock();
        let mut bm = db::load_bitmap(&conn, title_id, ver)
            .map_err(err)?
            .map(|(b, _, _)| b)
            .unwrap_or_else(|| blt_core::bitmap::Bitmap::new(total));
        // Mark all good chunks present, bad chunks absent.
        for i in 0..total {
            bm.set(i);
        }
        for i in &bad {
            bm.clear(*i);
        }
        db::save_download(&conn, title_id, ver, &bm, "paused", &dest, None).map_err(err)?;
    }
    if n > 0 {
        state
            .downloads
            .resume(state.inner(), &app, title_id, ver, title_name)?;
    }
    Ok(n)
}

/// Launch a title via its `launch` block entry (F16.7-.9).
#[tauri::command]
pub async fn launch_title(
    state: State<'_, Shared>,
    title_id: u64,
    info_hash: Option<String>,
    entry_index: usize,
) -> Cmd<()> {
    let info = fetch_title_info(state.clone(), title_id, info_hash).await?;
    let entry = info
        .launch
        .get(entry_index)
        .ok_or("no such launch entry")?
        .clone();
    let dest = {
        let conn = state.db.lock();
        db::title_location(&conn, title_id)
            .map_err(err)?
            .ok_or("title location unknown")?
    };
    let root = PathBuf::from(dest);
    let exe = blt_core::transfer::safe_join(&root, &entry.exe).map_err(err)?;
    let cwd = match &entry.cwd {
        Some(c) if c != "." => blt_core::transfer::safe_join(&root, c).map_err(err)?,
        _ => root.clone(),
    };
    info!(title = title_id, exe = %exe.display(), "launching title");
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.current_dir(&cwd);
    if let Some(args) = &entry.args {
        cmd.args(args.split_whitespace());
    }
    cmd.spawn().map_err(|e| format!("launch failed: {e}"))?;
    Ok(())
}

// ──────────────────────────── scripts ───────────────────────────────

#[tauri::command]
pub async fn script_preview(
    state: State<'_, Shared>,
    title_id: u64,
) -> Cmd<Option<scripts::ScriptPreview>> {
    scripts::fetch_preview(state.inner(), title_id).await
}

#[tauri::command]
pub async fn script_run(
    state: State<'_, Shared>,
    title_id: u64,
    expected_hash: String,
) -> Cmd<scripts::ScriptResult> {
    let dest = {
        let conn = state.db.lock();
        db::title_location(&conn, title_id)
            .map_err(err)?
            .ok_or("title location unknown")?
    };
    scripts::run_confirmed(
        state.inner(),
        title_id,
        &PathBuf::from(dest),
        &expected_hash,
    )
    .await
}

// ───────────────────────────── shares ───────────────────────────────

#[tauri::command]
pub async fn shares_list(state: State<'_, Shared>) -> Cmd<Vec<blt_core::protocol::ShareSummary>> {
    let share = state
        .connection
        .read()
        .share_endpoint
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or("share service not available")?;
    server_api::list_shares(&share).await.map_err(err)
}

#[tauri::command]
pub async fn share_listing(
    state: State<'_, Shared>,
    share_id: u64,
) -> Cmd<blt_core::protocol::ShareListing> {
    let share = state
        .connection
        .read()
        .share_endpoint
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or("share service not available")?;
    server_api::share_listing(&share, share_id)
        .await
        .map_err(err)
}

/// Upload dropped paths (UI confirmed first, #5). A directory becomes one
/// folder-share; loose files each become a file-share (F6.2-.4).
#[tauri::command]
pub async fn share_upload(state: State<'_, Shared>, paths: Vec<String>) -> Cmd<usize> {
    let share = state
        .connection
        .read()
        .share_endpoint
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or("share service not available")?;
    let (client_id, owner) = {
        let s = state.settings.read();
        (s.client_id.clone(), s.display_name.clone())
    };
    // Progress indicator: pre-flight the total bytes per path (upload reports at
    // per-item granularity — within-item % needs a streaming multipart body).
    let path_bytes: Vec<u64> = paths
        .iter()
        .map(|p| {
            let path = PathBuf::from(p);
            if path.is_dir() {
                let mut files = Vec::new();
                let _ = collect_tree(&path, &path, &mut files);
                files
                    .iter()
                    .filter_map(|(a, _)| std::fs::metadata(a).ok())
                    .map(|m| m.len())
                    .sum()
            } else {
                std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
            }
        })
        .collect();
    let total: u64 = path_bytes.iter().sum();
    let label = if paths.len() == 1 {
        PathBuf::from(&paths[0])
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("upload")
            .to_string()
    } else {
        format!("{} items", paths.len())
    };
    let xfer = state.xfer_begin("share-upload", &label, total);
    let cancel = state.xfer_cancel_flag(xfer).unwrap_or_default();

    let mut uploaded = 0usize;
    let mut done = 0u64;
    let up: Result<(), String> = async {
        for (idx, p) in paths.iter().enumerate() {
            if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                break; // cancelled between files
            }
            let path = PathBuf::from(p);
            let (name, kind, files): (String, &str, Vec<(PathBuf, String)>) = if path.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("Shared Folder")
                    .to_string();
                let mut files = Vec::new();
                collect_tree(&path, &path, &mut files).map_err(err)?;
                (name, "folder", files)
            } else if path.is_file() {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
                    .to_string();
                (name.clone(), "file", vec![(path.clone(), name)])
            } else {
                continue;
            };

            // Stream the upload and poll the sent-byte counter into the transfer
            // indicator (live % + speed for uploads, not just per-item).
            let sent = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let base_now = done;
            let st = state.inner().clone();
            let counter = sent.clone();
            let poll = tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    let s = counter.load(std::sync::atomic::Ordering::SeqCst);
                    st.xfer_update(xfer, base_now + s);
                }
            });
            let res = server_api::upload_share(
                &share,
                &client_id,
                &name,
                kind,
                &owner,
                &files,
                sent,
                cancel.clone(),
            )
            .await;
            poll.abort();
            if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                break; // cancelled mid-file → res is the aborted-stream error; ignore
            }
            res.map_err(err)?;
            uploaded += 1;
            done += path_bytes[idx];
            state.xfer_update(xfer, done);
        }
        Ok(())
    }
    .await;
    state.xfer_end(xfer);
    up?;
    Ok(uploaded)
}

fn collect_tree(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<(PathBuf, String)>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_tree(root, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            out.push((path, rel));
        }
    }
    Ok(())
}

#[derive(Serialize)]
pub struct ShareDownloadOut {
    pub total: usize,
    pub present: usize,
    pub missing: Vec<String>,
}

/// Download a share into `dest_dir` (UI confirmed + pre-flighted). Receiving
/// side sanitises every path before writing (HARD CONSTRAINT #11), then runs
/// the quick "X of N" completeness check (F6.8). `only_missing` refetches an
/// incomplete share's gaps.
#[tauri::command]
pub async fn share_download(
    state: State<'_, Shared>,
    share_id: u64,
    dest_dir: String,
    only_missing: bool,
) -> Cmd<ShareDownloadOut> {
    let share = state
        .connection
        .read()
        .share_endpoint
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or("share service not available")?;
    let listing = server_api::share_listing(&share, share_id)
        .await
        .map_err(err)?;
    // A single-file share writes straight into the chosen folder; only folder
    // shares get their own sub-folder (avoid `movie.mp4/movie.mp4`).
    let root = match listing.share.kind {
        blt_core::protocol::ShareKind::File => PathBuf::from(&dest_dir),
        blt_core::protocol::ShareKind::Folder => PathBuf::from(&dest_dir).join(
            sanitize_rel_path(&listing.share.name).unwrap_or_else(|_| format!("share-{share_id}")),
        ),
    };

    // Sanitise + de-collide all paths on OUR side before any write (#11).
    let mut used: HashSet<String> = HashSet::new();
    let mut plan: Vec<(String, String, u64)> = Vec::new(); // (remote rel, safe rel, size)
    for f in &listing.files {
        let safe = sanitize_rel_path(&f.rel_path).map_err(err)?;
        let safe = resolve_collision(&mut used, &safe);
        plan.push((f.rel_path.clone(), safe, f.size));
    }

    // Progress indicator (sidebar + title bar): total bytes across the plan.
    let total: u64 = plan.iter().map(|(_, _, s)| *s).sum();
    let xfer = state.xfer_begin("share-download", &listing.share.name, total);
    let cancel = state.xfer_cancel_flag(xfer).unwrap_or_default();
    let mut base = 0u64;
    let dl: Result<(), String> = async {
        for (remote_rel, safe_rel, size) in &plan {
            if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                break; // cancelled (between files)
            }
            let dest = blt_core::transfer::safe_join(&root, safe_rel).map_err(err)?;
            if only_missing {
                if let Ok(meta) = std::fs::metadata(&dest) {
                    if meta.len() == *size {
                        base += *size;
                        state.xfer_update(xfer, base);
                        continue; // already complete
                    }
                }
            }
            server_api::download_share_file(&share, share_id, remote_rel, &dest, |w| {
                state.xfer_update(xfer, base + w);
            })
            .await
            .map_err(err)?;
            base += *size;
            state.xfer_update(xfer, base);
        }
        Ok(())
    }
    .await;
    state.xfer_end(xfer);
    dl?;

    // Quick completeness: every expected file present at expected size (F6.8).
    let entries: Vec<(String, u64)> = plan
        .iter()
        .map(|(_, safe, size)| (safe.clone(), *size))
        .collect();
    let report = blt_core::transfer::completeness_check(&entries, &root);
    Ok(ShareDownloadOut {
        total: report.total,
        present: report.present,
        missing: report.missing,
    })
}

#[tauri::command]
pub async fn share_delete(state: State<'_, Shared>, share_id: u64) -> Cmd<()> {
    let share = state
        .connection
        .read()
        .share_endpoint
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or("share service not available")?;
    let client_id = state.settings.read().client_id.clone();
    server_api::delete_share(&share, &client_id, share_id)
        .await
        .map_err(err)
}

#[tauri::command]
pub fn preflight_dest(dest: String, needed: u64) -> (bool, Option<u64>) {
    freespace::preflight(&PathBuf::from(dest), needed)
}

// ──────────────────────── jukebox / playback ────────────────────────

#[tauri::command]
pub fn jukebox_state(state: State<'_, Shared>) -> Option<blt_core::protocol::JukeboxState> {
    state.live.read().jukebox.clone()
}

#[tauri::command]
pub fn jukebox_add(
    state: State<'_, Shared>,
    item_type: String,
    reference: String,
    title: Option<String>,
) -> Cmd<()> {
    let ty = ItemType::from_str_opt(&item_type).ok_or("unknown item type")?;
    state.send_ws(ClientMsg::JukeboxAdd {
        item_type: ty,
        reference,
        title,
    });
    Ok(())
}

#[tauri::command]
pub fn jukebox_vote(state: State<'_, Shared>, item_id: u64) {
    state.send_ws(ClientMsg::JukeboxVote { item_id });
}

#[tauri::command]
pub fn jukebox_next(state: State<'_, Shared>) -> Cmd<()> {
    if state.settings.read().mode != Mode::Playback {
        return Err("only the playback machine advances the queue".into());
    }
    state.send_ws(ClientMsg::JukeboxNext);
    Ok(())
}

#[tauri::command]
pub fn jukebox_ended(state: State<'_, Shared>) -> Cmd<()> {
    if state.settings.read().mode != Mode::Playback {
        return Err("only the playback machine reports ended".into());
    }
    state.send_ws(ClientMsg::JukeboxEnded);
    Ok(())
}

#[tauri::command]
pub fn roster(state: State<'_, Shared>) -> Vec<blt_core::protocol::RosterEntry> {
    state.live.read().roster.clone()
}

/// Resolve a shared-file queue item (`ref` = share id) to a streamable URL
/// (F9.2): pick the largest video-looking file in the share.
#[tauri::command]
pub async fn resolve_share_stream(state: State<'_, Shared>, share_id: u64) -> Cmd<String> {
    let share = state
        .connection
        .read()
        .share_endpoint
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or("share service not available")?;
    let listing = server_api::share_listing(&share, share_id)
        .await
        .map_err(err)?;
    const VIDEO_EXT: &[&str] = &["mp4", "webm", "mkv", "mov", "m4v", "avi", "ogv"];
    let best = listing
        .files
        .iter()
        .filter(|f| {
            f.rel_path
                .rsplit('.')
                .next()
                .map(|e| VIDEO_EXT.contains(&e.to_ascii_lowercase().as_str()))
                .unwrap_or(false)
        })
        .max_by_key(|f| f.size)
        .or_else(|| listing.files.iter().max_by_key(|f| f.size))
        .ok_or("share has no files")?;
    let encoded = best
        .rel_path
        .split('/')
        .map(|s| s.replace('%', "%25").replace(' ', "%20"))
        .collect::<Vec<_>>()
        .join("/");
    Ok(format!("http://{share}/shares/{share_id}/files/{encoded}"))
}

// ───────────────────────── lockdown (F14) ───────────────────────────

/// Enter playback lockdown — admin-password gated via the server (F14.3).
#[tauri::command]
pub async fn lockdown_enter(
    state: State<'_, Shared>,
    app: tauri::AppHandle,
    password: String,
) -> Cmd<()> {
    verify_admin(&state, &password).await?;
    {
        let mut s = state.settings.write();
        s.mode = Mode::Playback;
        s.playback_locked = true;
        let conn = state.db.lock();
        s.persist(&conn).map_err(err)?;
        db::set_setting(&conn, "mode_chosen", "true").map_err(err)?;
    }
    info!("entering playback lockdown (restart)");
    app.restart();
}

/// Exit lockdown — same gate; without the password the machine stays locked.
#[tauri::command]
pub async fn lockdown_exit(
    state: State<'_, Shared>,
    app: tauri::AppHandle,
    password: String,
) -> Cmd<()> {
    verify_admin(&state, &password).await?;
    {
        let mut s = state.settings.write();
        s.playback_locked = false;
        s.mode = Mode::Client;
        let conn = state.db.lock();
        s.persist(&conn).map_err(err)?;
    }
    info!("exiting playback lockdown (restart)");
    app.restart();
}

async fn verify_admin(state: &State<'_, Shared>, password: &str) -> Cmd<()> {
    let game =
        state.connection.read().game_endpoint.clone().ok_or(
            "not connected to the server (the lockdown gate checks the admin password there)",
        )?;
    match server_api::verify_admin_password(&game, password).await {
        Ok(true) => Ok(()),
        Ok(false) => Err("wrong admin password".into()),
        Err(e) => Err(format!("could not verify with the server: {e}")),
    }
}

// ─────────────────────────────── log ────────────────────────────────

/// Tail of this client's own log (F17.3).
#[tauri::command]
pub async fn log_tail(state: State<'_, Shared>, lines: usize) -> Cmd<Vec<String>> {
    let logs_dir = state.dirs.logs.clone();
    let lines = lines.min(5000);
    tokio::task::spawn_blocking(move || -> std::io::Result<Vec<String>> {
        let mut files: Vec<_> = std::fs::read_dir(&logs_dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        let Some(newest) = files.last() else {
            return Ok(Vec::new());
        };
        let content = std::fs::read_to_string(newest)?;
        Ok(content
            .lines()
            .rev()
            .take(lines)
            .map(String::from)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect())
    })
    .await
    .map_err(err)?
    .map_err(err)
}

/// External/DRM launch on the playback machine (F10.1): opens the real
/// browser/app. The confirm step happened in the initiating UI (#5).
#[tauri::command]
pub fn external_open(app: tauri::AppHandle, url: String) -> Cmd<()> {
    use tauri_plugin_opener::OpenerExt;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("only http(s) links can be opened externally".into());
    }
    app.opener()
        .open_url(url, None::<String>)
        .map_err(|e| format!("open failed: {e}"))
}

// ───────────────────────── self-update (F0.4) ───────────────────────

#[derive(Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: Option<String>,
}

/// Check GitHub Releases for a newer signed version. **Manual semantics**
/// (HARD CONSTRAINT #2): this only *reports* — nothing is downloaded or
/// installed here. Offline / unreachable → the caller treats the error as a
/// graceful no-op.
#[tauri::command]
pub async fn update_check(app: tauri::AppHandle) -> Cmd<Option<UpdateInfo>> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await {
        Ok(Some(update)) => {
            info!(version = %update.version, "update available");
            Ok(Some(UpdateInfo {
                version: update.version.clone(),
                notes: update.body.clone(),
            }))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(format!("update check failed: {e}")),
    }
}

/// "Download & restart" — runs only on the user's explicit click (#2). The
/// bundle's Ed25519 signature is verified by the updater before install; the
/// app restarts only after a successful install.
#[tauri::command]
pub async fn update_install(state: State<'_, Shared>, app: tauri::AppHandle) -> Cmd<()> {
    use tauri_plugin_updater::UpdaterExt;
    // Belt-and-braces: a locked playback client never updates from in-app UI —
    // the admin unlocks it first (F0.4/F14.4).
    if state.settings.read().playback_locked {
        return Err("this machine is locked to playback — unlock it first to update".into());
    }
    let updater = app.updater().map_err(|e| e.to_string())?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        return Err("no update available".into());
    };
    info!(version = %update.version, "downloading + installing update (user-initiated)");
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| format!("update failed (nothing was changed): {e}"))?;
    info!("update installed — restarting");
    app.restart();
}
