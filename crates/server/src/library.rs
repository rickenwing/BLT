//! Library scan → manifest publish, sidecar metadata, and the staging/settle
//! promotion workflow (TDD §3.3, F2).
//!
//! - Each immediate subfolder of the library root = one title (F2.1).
//! - The `.blt/` sidecar is the admin's **input** format: `info.json` +
//!   `cover.*` + optional Windows scripts. It is **excluded from the
//!   distributable manifest** (HARD CONSTRAINT #10); referenced install scripts
//!   are delivered separately via `/titles/{id}/script` (TDD §5.2).
//! - Re-scan change detection: size+mtime fast check, hashing only changed
//!   files (F2.6). A game-file change bumps `manifest_ver`; a sidecar-only
//!   change bumps `info_hash` only.
//! - Staging: a title is promoted into the library (atomic same-volume rename)
//!   only after its file tree is stable for the settle window (F2.2).

use crate::state::SharedState;
use crate::util::now_unix;
use crate::{db, error::ApiError};
use base64::Engine;
use blt_core::hashing::{hash_bytes, Hash, StreamHasher};
use blt_core::manifest::{build_file_entry, FileEntry};
use blt_core::protocol::{ServerMsg, SidecarInfo, TitleInfo};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::Instant;
use tracing::{error, info, warn};

/// Name of the metadata sidecar folder inside a title.
pub const SIDECAR_DIR: &str = ".blt";

/// Result of one full library scan.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ScanSummary {
    pub scanned: usize,
    pub published: Vec<String>,
    pub republished: Vec<String>,
    pub info_updated: Vec<String>,
    pub removed: Vec<String>,
    pub errors: Vec<String>,
}

/// Scan the whole library (manual "Scan now" + periodic auto-scan, F2.5).
/// Blocking (file IO + hashing) — callers run it via `spawn_blocking`.
pub fn scan_library(state: &SharedState) -> Result<ScanSummary, ApiError> {
    let Some(library) = state.library_path() else {
        return Err(ApiError::BadRequest("library path not configured".into()));
    };
    if !library.is_dir() {
        return Err(ApiError::BadRequest(format!(
            "library path {} is not a directory",
            library.display()
        )));
    }

    let mut summary = ScanSummary::default();
    let mut seen: Vec<String> = Vec::new();

    let entries = fs::read_dir(&library)?;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                summary.errors.push(format!("read_dir: {e}"));
                continue;
            }
        };
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(String::from) else {
            warn!("skipping non-UTF8 folder name in library");
            continue;
        };
        if !path.is_dir() || name.starts_with('.') {
            continue;
        }
        seen.push(name.clone());
        summary.scanned += 1;
        match scan_title(state, &library, &name) {
            Ok(TitleScanOutcome::Published) => summary.published.push(name),
            Ok(TitleScanOutcome::Republished) => summary.republished.push(name),
            Ok(TitleScanOutcome::InfoUpdated) => summary.info_updated.push(name),
            Ok(TitleScanOutcome::Unchanged) => {}
            Err(e) => {
                error!(title = %name, "scan failed: {e}");
                summary.errors.push(format!("{name}: {e}"));
            }
        }
    }

    // Titles in the DB but no longer on disk → removed (F2.5).
    let removed: Vec<(u64, String)> = {
        let conn = state.db.lock();
        db::list_titles(&conn)?
            .into_iter()
            .filter(|t| !seen.contains(&t.name))
            .map(|t| (t.id, t.name))
            .collect()
    };
    for (id, name) in removed {
        let conn = state.db.lock();
        db::set_title_state(&conn, id, "removed")?;
        drop(conn);
        state.manifests.write().remove(&id);
        info!(title = %name, "title removed (folder gone)");
        summary.removed.push(name);
    }

    info!(
        scanned = summary.scanned,
        published = summary.published.len(),
        republished = summary.republished.len(),
        info_updated = summary.info_updated.len(),
        removed = summary.removed.len(),
        errors = summary.errors.len(),
        "library scan complete"
    );
    Ok(summary)
}

enum TitleScanOutcome {
    Published,
    Republished,
    InfoUpdated,
    Unchanged,
}

/// Scan one title folder; publish/republish as needed.
fn scan_title(
    state: &SharedState,
    library: &Path,
    name: &str,
) -> Result<TitleScanOutcome, ApiError> {
    let title_dir = library.join(name);
    let chunk_size = state.config.read().chunk_size;

    let title_id = {
        let conn = state.db.lock();
        db::ensure_title(&conn, name)?
    };

    // Existing manifest (for fast size+mtime change detection, F2.6).
    // A freshly-inserted title row has manifest_ver 0 — that is NOT a previous
    // publish, so a first scan reports "published", not "republished". A
    // 'removed' title whose folder reappeared is likewise treated as new:
    // the unchanged-early-return path would otherwise leave it unpublished
    // forever (F2.5).
    let previous = {
        let conn = state.db.lock();
        let has_manifest = db::get_title(&conn, title_id)?
            .map(|t| t.manifest_ver > 0 && t.state == "published")
            .unwrap_or(false);
        if has_manifest {
            drop(conn);
            state.cached_manifest(title_id).or_else(|| {
                let conn = state.db.lock();
                db::load_manifest(&conn, title_id)
                    .ok()
                    .flatten()
                    .map(std::sync::Arc::new)
            })
        } else {
            None
        }
    };
    let prev_by_path: HashMap<&str, &FileEntry> = previous
        .as_deref()
        .map(|m| m.files.iter().map(|f| (f.rel_path.as_str(), f)).collect())
        .unwrap_or_default();

    // Walk the tree, excluding the .blt/ sidecar (HARD CONSTRAINT #10).
    let mut walked: Vec<(String, u64, i64)> = Vec::new(); // (rel, size, mtime)
    for e in walkdir::WalkDir::new(&title_dir)
        .follow_links(false)
        .sort_by_file_name()
    {
        let e = e.map_err(|err| ApiError::Internal(format!("walk: {err}")))?;
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e
            .path()
            .strip_prefix(&title_dir)
            .map_err(|_| ApiError::Internal("strip_prefix".into()))?;
        let rel_str = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        // Exclude the sidecar from the distributable manifest. Compared
        // case-insensitively: on the case-insensitive filesystems we target a
        // ".BLT" folder IS the sidecar and must never leak into the manifest.
        let rel_lower = rel_str.to_ascii_lowercase();
        if rel_lower == SIDECAR_DIR || rel_lower.starts_with(&format!("{SIDECAR_DIR}/")) {
            continue;
        }
        let meta = e
            .metadata()
            .map_err(|err| ApiError::Internal(format!("stat: {err}")))?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        walked.push((rel_str, meta.len(), mtime));
    }
    walked.sort(); // stable manifest order (bitmap order stability)

    // Determine changed set + build entries, hashing only new/changed files.
    let mut files: Vec<FileEntry> = Vec::with_capacity(walked.len());
    let mut any_change = previous.is_none();
    if let Some(prev) = previous.as_deref() {
        if prev.files.len() != walked.len() {
            any_change = true;
        }
    }
    for (rel, size, mtime) in &walked {
        let prev = prev_by_path.get(rel.as_str());
        if let Some(prev) = prev {
            if prev.size == *size && prev.mtime == *mtime {
                // Fast path: size+mtime match → reuse previous hashes (the id is
                // reassigned by the DB upsert).
                files.push((*prev).clone());
                continue;
            }
        }
        // Size or mtime differs (or new file): re-hash to find out for sure.
        let entry = hash_one_file(&title_dir, rel, *size, *mtime, chunk_size)?;
        // OBS-1: a new mtime with identical content (e.g. a `touch`, or a folder
        // regenerated byte-for-byte) is NOT a real change — keep the previous
        // entry so we don't bump manifest_ver and force every client to
        // re-download unchanged bytes. Only genuine content/size changes count.
        match prev {
            Some(p) if p.size == *size && p.hash == entry.hash => {
                files.push((*p).clone());
            }
            _ => {
                any_change = true;
                files.push(entry);
            }
        }
    }

    // Sidecar → title-info payload + info_hash (decoupled from the manifest).
    let (info_hash, info_json, has_cover, has_install_script) = build_info_payload(&title_dir)?;

    let prev_info_hash = {
        let conn = state.db.lock();
        db::get_title(&conn, title_id)?.and_then(|t| t.info_hash)
    };
    let info_changed = prev_info_hash != info_hash;

    let was_published = previous.is_some();
    if !any_change && !info_changed && was_published {
        return Ok(TitleScanOutcome::Unchanged);
    }

    let total_size: u64 = files.iter().map(|f| f.size).sum();
    let file_count = files.len() as u64;
    let now = now_unix();

    if any_change {
        // Bump manifest version — versions are immutable snapshots (F4.9).
        let new_version = previous.as_deref().map(|m| m.manifest_ver + 1).unwrap_or(1);
        {
            let mut conn = state.db.lock();
            db::replace_manifest(
                &mut conn,
                title_id,
                &files,
                total_size,
                file_count,
                new_version,
                now,
                info_hash.as_ref(),
                info_json.as_deref(),
                has_cover,
                has_install_script,
            )?;
        }
        // Reload with DB-assigned stable file ids and cache for serving.
        let manifest = {
            let conn = state.db.lock();
            db::load_manifest(&conn, title_id)?
        };
        if let Some(m) = manifest {
            if let Err(e) = m.validate_structure() {
                return Err(ApiError::Internal(format!("manifest inconsistent: {e}")));
            }
            state.cache_manifest(m);
        }
        // Tell clients a title changed (update-available state, F4.9).
        state.sessions.lock().broadcast(ServerMsg::TitleChanged {
            title_id,
            manifest_ver: new_version,
        });
        info!(title = %name, version = new_version, files = file_count, bytes = total_size,
              "title {}", if was_published { "republished" } else { "published" });
        Ok(if was_published {
            TitleScanOutcome::Republished
        } else {
            TitleScanOutcome::Published
        })
    } else {
        // Sidecar-only change: update info payload without bumping manifest_ver
        // (HARD CONSTRAINT #10 / F2.6).
        let conn = state.db.lock();
        db::update_info_only(
            &conn,
            title_id,
            info_hash.as_ref(),
            info_json.as_deref(),
            has_cover,
            has_install_script,
            now,
        )?;
        info!(title = %name, "title info/cover updated (manifest version unchanged)");
        Ok(TitleScanOutcome::InfoUpdated)
    }
}

/// Hash one file in a single read pass: whole-file hash + per-chunk hashes.
fn hash_one_file(
    title_dir: &Path,
    rel: &str,
    size: u64,
    mtime: i64,
    chunk_size: u64,
) -> Result<FileEntry, ApiError> {
    let path = title_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    let mut f = fs::File::open(&path)?;
    let mut whole = StreamHasher::new();
    let mut chunk_hashes: Vec<Hash> = Vec::new();
    let mut buf = vec![0u8; chunk_size as usize];
    loop {
        // Fill up to one chunk.
        let mut filled = 0usize;
        while filled < buf.len() {
            let n = f.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        whole.update(&buf[..filled]);
        chunk_hashes.push(hash_bytes(&buf[..filled]));
        if filled < buf.len() {
            break; // EOF
        }
    }
    Ok(build_file_entry(
        0, // real id assigned by the DB upsert
        rel.to_string(),
        size,
        mtime,
        whole.finalize(),
        chunk_size,
        &chunk_hashes,
    ))
}

/// Read the `.blt/` sidecar and build the served [`TitleInfo`] payload.
/// Returns `(info_hash, info_json, has_cover, has_install_script)` — all
/// `None`/false when there is no sidecar (graceful fallback, F2.3).
fn build_info_payload(
    title_dir: &Path,
) -> Result<(Option<Hash>, Option<String>, bool, bool), ApiError> {
    let sidecar = title_dir.join(SIDECAR_DIR);
    if !sidecar.is_dir() {
        return Ok((None, None, false, false));
    }

    let mut info: TitleInfo = TitleInfo::default();
    let mut has_install_script = false;

    let info_path = sidecar.join("info.json");
    if info_path.is_file() {
        match fs::read_to_string(&info_path)
            .map_err(ApiError::from)
            .and_then(|s| {
                serde_json::from_str::<SidecarInfo>(&s)
                    .map_err(|e| ApiError::BadRequest(format!("info.json: {e}")))
            }) {
            Ok(side) => {
                has_install_script = side
                    .install_script
                    .as_ref()
                    .and_then(|s| s.windows.as_ref())
                    .is_some();
                info = TitleInfo {
                    name: side.name,
                    year: side.year,
                    genre: side.genre,
                    players: side.players,
                    blurb: side.blurb,
                    link: side.link,
                    cover_b64: None,
                    launch: side.launch,
                    install_script: side.install_script,
                };
            }
            Err(e) => {
                // Malformed metadata must not block publishing the game itself.
                warn!("ignoring malformed {}: {e}", info_path.display());
            }
        }
    }

    // cover.* image → embedded base64 (F2.4).
    let mut has_cover = false;
    if let Ok(entries) = fs::read_dir(&sidecar) {
        for e in entries.flatten() {
            let p = e.path();
            let stem_is_cover = p
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("cover"));
            let ext_ok = p.extension().and_then(|s| s.to_str()).is_some_and(|s| {
                matches!(
                    s.to_ascii_lowercase().as_str(),
                    "png" | "jpg" | "jpeg" | "webp" | "gif"
                )
            });
            if p.is_file() && stem_is_cover && ext_ok {
                match fs::read(&p) {
                    Ok(bytes) => {
                        info.cover_b64 =
                            Some(base64::engine::general_purpose::STANDARD.encode(bytes));
                        has_cover = true;
                    }
                    Err(e) => warn!("cover unreadable {}: {e}", p.display()),
                }
                break;
            }
        }
    }

    let json = serde_json::to_string(&info)
        .map_err(|e| ApiError::Internal(format!("info serialize: {e}")))?;
    let hash = hash_bytes(json.as_bytes());
    Ok((Some(hash), Some(json), has_cover, has_install_script))
}

/// Serve-side lookup of a title's install script (TDD §5.2): returns
/// `(file_name, bytes)` if the title defines one. Canonical-library only.
pub fn read_install_script(
    state: &SharedState,
    title_id: u64,
) -> Result<Option<(String, Vec<u8>)>, ApiError> {
    let (name, script_rel) = {
        let conn = state.db.lock();
        let Some(title) = db::get_title(&conn, title_id)? else {
            return Ok(None);
        };
        let Some(info_json) = db::get_info_json(&conn, title_id)? else {
            return Ok(None);
        };
        let info: TitleInfo = serde_json::from_str(&info_json)
            .map_err(|e| ApiError::Internal(format!("stored info: {e}")))?;
        let Some(rel) = info.install_script.and_then(|s| s.windows) else {
            return Ok(None);
        };
        (title.name, rel)
    };
    let Some(library) = state.library_path() else {
        return Ok(None);
    };
    // The script lives inside the sidecar; refuse path tricks.
    if script_rel.contains("..")
        || script_rel.contains('/')
        || script_rel.contains('\\')
        || script_rel.contains(':')
    {
        // ':' covers Windows drive-relative prefixes and NTFS alternate data
        // streams; the script must be a plain filename inside the sidecar.
        return Err(ApiError::BadRequest("invalid script path".into()));
    }
    let path = library.join(&name).join(SIDECAR_DIR).join(&script_rel);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some((script_rel, fs::read(path)?)))
}

// ───────────────────────── staging / settle ─────────────────────────

/// One staged folder's observed signature: total size, file count, and the
/// max mtime seen — any change resets the settle clock (F2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
struct StageSignature {
    total_size: u64,
    file_count: u64,
    max_mtime: i64,
}

/// In-memory settle tracker for the staging watcher task.
#[derive(Default)]
pub struct StagingTracker {
    observed: HashMap<String, (StageSignature, Instant)>,
}

impl StagingTracker {
    /// Inspect the staging dir; promote any folder whose signature has been
    /// stable for `settle_secs`. Returns the names promoted this tick.
    pub fn tick(&mut self, state: &SharedState) -> Vec<String> {
        let Some(staging) = state.staging_path() else {
            return Vec::new();
        };
        let Some(library) = state.library_path() else {
            return Vec::new();
        };
        let settle_secs = state.config.read().staging_settle_secs;
        let mut promoted = Vec::new();

        let entries = match fs::read_dir(&staging) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
        let mut present: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            if !path.is_dir() || name.starts_with('.') {
                continue;
            }
            present.push(name.clone());
            let sig = match signature_of(&path) {
                Ok(s) => s,
                Err(e) => {
                    warn!(title = %name, "staging stat failed: {e}");
                    continue;
                }
            };
            match self.observed.get(&name) {
                Some((prev, since)) if *prev == sig => {
                    if since.elapsed().as_secs() >= settle_secs {
                        // Stable: promote via atomic same-volume rename (F2.2).
                        let dest = library.join(&name);
                        if dest.exists() {
                            // Updating an existing title: replace its tree.
                            // DESIGN-NOTE: the docs accept the rare
                            // mid-update-while-downloading window (TDD §3.3);
                            // we move the old tree aside, rename the staged one
                            // in, then delete the old tree.
                            let old = library.join(format!(".{name}.replaced"));
                            let _ = fs::remove_dir_all(&old);
                            if let Err(e) = fs::rename(&dest, &old) {
                                error!(title = %name, "promote: cannot move old tree: {e}");
                                continue;
                            }
                            if let Err(e) = fs::rename(&path, &dest) {
                                error!(title = %name, "promote rename failed: {e}; restoring old");
                                let _ = fs::rename(&old, &dest);
                                continue;
                            }
                            let _ = fs::remove_dir_all(&old);
                        } else if let Err(e) = fs::rename(&path, &dest) {
                            error!(title = %name, "promote rename failed: {e} (staging must be on the library volume)");
                            continue;
                        }
                        info!(title = %name, "staged title promoted to library");
                        self.observed.remove(&name);
                        promoted.push(name);
                    }
                }
                _ => {
                    // New or changed: (re)start the settle clock.
                    self.observed.insert(name, (sig, Instant::now()));
                }
            }
        }
        // Forget folders that disappeared from staging.
        self.observed.retain(|k, _| present.contains(k));
        promoted
    }
}

fn signature_of(dir: &Path) -> std::io::Result<StageSignature> {
    let mut total_size = 0u64;
    let mut file_count = 0u64;
    let mut max_mtime = 0i64;
    for e in walkdir::WalkDir::new(dir).follow_links(false) {
        let e = e.map_err(|err| std::io::Error::other(format!("walk: {err}")))?;
        if !e.file_type().is_file() {
            continue;
        }
        let meta = e
            .metadata()
            .map_err(|err| std::io::Error::other(format!("stat: {err}")))?;
        total_size += meta.len();
        file_count += 1;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        max_mtime = max_mtime.max(mtime);
    }
    Ok(StageSignature {
        total_size,
        file_count,
        max_mtime,
    })
}
