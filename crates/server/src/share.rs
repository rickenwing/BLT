//! Shared file pool (F6, TDD §5.1) — its own listener/NIC, server-direct only
//! (no P2P, F6.11).
//!
//! A share is a **file or a folder tree**, uploaded as multipart where each
//! part's `filename` carries the file's relative path. Every received path is
//! **sanitised on the writing side** (HARD CONSTRAINT #11) — traversal,
//! OS-illegal characters, length, and case-insensitive collisions are all
//! handled here, so a folder authored on one OS lands safely on the share
//! drive and later on any downloader's OS.

use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::state::SharedState;
use crate::util::now_unix;
use axum::extract::{Multipart, Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use blt_core::pathsafe::{resolve_collision, sanitize_rel_path};
use blt_core::protocol::{ShareKind, ShareListing, ShareSummary};
use std::collections::HashSet;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::{info, warn};

/// Header carrying the uploader's client id (advisory identity; trusted LAN).
pub const CLIENT_ID_HEADER: &str = "x-blt-client-id";

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/shares", get(list_shares).post(upload_share))
        .route("/shares/{id}", get(share_listing).delete(delete_share))
        .route("/shares/{id}/files/{*rel}", get(serve_share_file))
        .with_state(state)
        // Folder uploads can be large; uploads stream to disk part by part.
        .layer(axum::extract::DefaultBodyLimit::disable())
}

async fn list_shares(State(state): State<SharedState>) -> ApiResult<Json<Vec<ShareSummary>>> {
    let conn = state.db.lock();
    Ok(Json(db::list_shares(&conn)?))
}

/// `GET /shares/{id}` — metadata + the file listing (folder tree recreate +
/// completeness check both come from this, F6.5/F6.8).
async fn share_listing(
    State(state): State<SharedState>,
    AxPath(id): AxPath<u64>,
) -> ApiResult<Json<ShareListing>> {
    let conn = state.db.lock();
    let share = db::get_share(&conn, id)?.ok_or(ApiError::NotFound)?;
    let files = db::share_files(&conn, id)?
        .into_iter()
        .map(|(rel_path, size)| blt_core::protocol::ShareFileEntry { rel_path, size })
        .collect();
    Ok(Json(ShareListing { share, files }))
}

/// `POST /shares` — upload a file or folder tree (F6.2-.4).
///
/// Multipart contract (produced by our own clients):
/// - optional `meta` part first: JSON `{ "name", "kind", "owner_name" }`
/// - one or more `file` parts; each part's `filename` is the file's
///   **relative path** within the share (URL-style `/` separators).
///
/// The uploader's client id rides the `x-blt-client-id` header.
async fn upload_share(
    State(state): State<SharedState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> ApiResult<Json<ShareSummary>> {
    let share_root = state
        .share_path()
        .ok_or_else(|| ApiError::BadRequest("share path not configured".into()))?;
    tokio::fs::create_dir_all(&share_root).await?;

    // Upload bounds (SEC-3): keep a malicious/runaway client from filling the
    // share volume or creating unbounded files.
    const MAX_UPLOAD_BYTES: u64 = 256 * 1024 * 1024 * 1024; // 256 GiB per upload
    const MAX_PARTS: usize = 50_000;
    const MIN_FREE_HEADROOM: u64 = 1024 * 1024 * 1024; // refuse if <1 GiB free
    if let Ok(free) = fs4::available_space(&share_root) {
        if free < MIN_FREE_HEADROOM {
            return Err(ApiError::BadRequest(format!(
                "share volume nearly full ({} MiB free)",
                free / (1024 * 1024)
            )));
        }
    }

    let owner_client = headers
        .get(CLIENT_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    #[derive(serde::Deserialize, Default)]
    struct Meta {
        name: Option<String>,
        kind: Option<String>,
        owner_name: Option<String>,
    }
    let mut meta = Meta::default();

    // Collision set primed with what's already on the share drive.
    let mut used: HashSet<String> = HashSet::new();
    if let Ok(mut rd) = tokio::fs::read_dir(&share_root).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            if let Some(n) = e.file_name().to_str() {
                used.insert(n.to_lowercase());
            }
        }
    }

    let mut files: Vec<(String, u64)> = Vec::new(); // rel paths within the share
    let mut total: u64 = 0;
    let mut share_dir_name: Option<String> = None; // for folder shares
    let mut single_file_name: Option<String> = None; // for file shares

    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("multipart: {e}")))?
    {
        match field.name() {
            Some("meta") => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| ApiError::BadRequest(format!("meta: {e}")))?;
                meta = serde_json::from_str(&text)
                    .map_err(|e| ApiError::BadRequest(format!("meta json: {e}")))?;
            }
            Some("file") => {
                if files.len() >= MAX_PARTS {
                    return Err(ApiError::BadRequest(format!(
                        "too many files in one upload (max {MAX_PARTS})"
                    )));
                }
                let raw_rel = field
                    .file_name()
                    .ok_or_else(|| ApiError::BadRequest("file part without filename".into()))?
                    .to_string();
                let is_folder = meta.kind.as_deref() == Some("folder");

                // Sanitise the *relative* path on the writing side (#11).
                let rel = sanitize_rel_path(&raw_rel)
                    .map_err(|e| ApiError::BadRequest(format!("bad path {raw_rel:?}: {e}")))?;

                let (dest, rel_recorded) = if is_folder {
                    // All files live under one share folder, named once.
                    let dir = match &share_dir_name {
                        Some(d) => d.clone(),
                        None => {
                            let base = meta
                                .name
                                .clone()
                                .unwrap_or_else(|| "Shared Folder".to_string());
                            let base = sanitize_rel_path(&base)
                                .unwrap_or_else(|_| "Shared Folder".to_string());
                            let unique = resolve_collision(&mut used, &base);
                            share_dir_name = Some(unique.clone());
                            unique
                        }
                    };
                    let mut within = HashSet::new(); // per-share collision set
                    within.extend(files.iter().map(|(r, _)| r.to_lowercase()));
                    let rel_unique = resolve_collision(&mut within, &rel);
                    (
                        blt_core::transfer::safe_join(&share_root.join(&dir), &rel_unique)
                            .map_err(|e| ApiError::BadRequest(format!("path: {e}")))?,
                        rel_unique,
                    )
                } else {
                    // Single-file share: flatten to the basename on the drive.
                    let base = rel.rsplit('/').next().unwrap_or(&rel).to_string();
                    let unique = resolve_collision(&mut used, &base);
                    single_file_name = Some(unique.clone());
                    (share_root.join(&unique), base)
                };

                if let Some(parent) = dest.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                let mut f = tokio::fs::File::create(&dest).await?;
                let mut written: u64 = 0;
                loop {
                    match field.chunk().await {
                        Ok(Some(bytes)) => {
                            written += bytes.len() as u64;
                            if total + written > MAX_UPLOAD_BYTES {
                                drop(f);
                                let _ = tokio::fs::remove_file(&dest).await;
                                return Err(ApiError::BadRequest(format!(
                                    "upload exceeds the {} GiB limit",
                                    MAX_UPLOAD_BYTES / (1024 * 1024 * 1024)
                                )));
                            }
                            f.write_all(&bytes).await?;
                        }
                        Ok(None) => break,
                        Err(e) => {
                            // Partial upload: remove the incomplete file.
                            drop(f);
                            let _ = tokio::fs::remove_file(&dest).await;
                            return Err(ApiError::BadRequest(format!("upload aborted: {e}")));
                        }
                    }
                }
                f.flush().await?;
                total += written;
                files.push((rel_recorded, written));
            }
            _ => continue,
        }
    }

    if files.is_empty() {
        return Err(ApiError::BadRequest("no files in upload".into()));
    }

    let kind = if meta.kind.as_deref() == Some("folder") {
        ShareKind::Folder
    } else {
        ShareKind::File
    };
    let owner_name = meta
        .owner_name
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown".to_string());
    let (display_name, stored) = match kind {
        ShareKind::Folder => {
            let dir = share_dir_name.unwrap_or_else(|| "Shared Folder".into());
            (
                meta.name.unwrap_or_else(|| dir.clone()),
                share_root.join(&dir),
            )
        }
        ShareKind::File => {
            let fname = single_file_name.unwrap_or_default();
            (fname.clone(), share_root.join(&fname))
        }
    };

    let id = {
        let mut conn = state.db.lock();
        db::insert_share(
            &mut conn,
            &display_name,
            kind,
            total,
            &owner_name,
            owner_client.as_deref(),
            &stored.to_string_lossy(),
            now_unix(),
            &files,
        )?
    };
    info!(share = %display_name, files = files.len(), bytes = total, owner = %owner_name, "share uploaded");

    let conn = state.db.lock();
    let summary = db::get_share(&conn, id)?.ok_or(ApiError::NotFound)?;
    Ok(Json(summary))
}

/// `GET /shares/{id}/files/{rel}` with Range — share download and the
/// playback machine's LAN-stream path (F9.2).
async fn serve_share_file(
    State(state): State<SharedState>,
    AxPath((id, rel)): AxPath<(u64, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    // One lock for both lookups on the same `shares` row (streaming hot path).
    // Delete race (F15.4): a vanished share yields a clear 410, not a hang/corrupt.
    let (stored_path, kind_is_file) = {
        let conn = state.db.lock();
        let Some((stored_path, _owner)) = db::share_stored_path(&conn, id)? else {
            return Ok((
                StatusCode::GONE,
                Json(serde_json::json!({"error": "share was deleted"})),
            )
                .into_response());
        };
        let kind_is_file = db::get_share(&conn, id)?
            .map(|s| s.kind == ShareKind::File)
            .unwrap_or(false);
        (stored_path, kind_is_file)
    };
    let base = PathBuf::from(&stored_path);
    let path = if kind_is_file {
        base.clone()
    } else {
        blt_core::transfer::safe_join(&base, &rel)
            .map_err(|e| ApiError::BadRequest(format!("path: {e}")))?
    };
    if !path.is_file() {
        return Ok((
            StatusCode::GONE,
            Json(serde_json::json!({"error": "file no longer exists (share deleted?)"})),
        )
            .into_response());
    }
    let size = tokio::fs::metadata(&path).await?.len();
    serve_range(&path, size, &headers).await
}

/// Range-capable file serving shared by the share endpoints.
async fn serve_range(path: &Path, size: u64, headers: &HeaderMap) -> ApiResult<Response> {
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| crate::data_api::parse_byte_range(s, size));
    let (start, len, status) = match range {
        Some((s, e)) => (s, e - s + 1, StatusCode::PARTIAL_CONTENT),
        None => (0, size, StatusCode::OK),
    };

    // Delete-race: open allowing concurrent delete so a share deleted mid-stream
    // isn't left orphaned on Windows (crate::util::open_read_shared).
    let mut f = tokio::fs::File::from_std(crate::util::open_read_shared(path)?);
    f.seek(SeekFrom::Start(start)).await?;
    // Stream in bounded pieces (shared video files can be GBs).
    let reader = f.take(len);
    let stream = tokio_util::io::ReaderStream::with_capacity(reader, 256 * 1024);
    let body = axum::body::Body::from_stream(stream);

    let mut resp = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len.to_string());
    if status == StatusCode::PARTIAL_CONTENT {
        resp = resp.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, start + len - 1, size),
        );
    }
    resp.body(body)
        .map_err(|e| ApiError::Internal(format!("response: {e}")))
}

/// `DELETE /shares/{id}` — uploader-only on this listener (the admin deletes
/// via the password-gated admin API, F6.9/F11.2). Removes files then the row.
async fn delete_share(
    State(state): State<SharedState>,
    AxPath(id): AxPath<u64>,
    headers: HeaderMap,
) -> ApiResult<StatusCode> {
    let caller = headers
        .get(CLIENT_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let stored = {
        let conn = state.db.lock();
        db::share_stored_path(&conn, id)?
    };
    let Some((stored_path, owner)) = stored else {
        return Err(ApiError::NotFound);
    };
    match owner.as_deref() {
        Some(o) if o == caller && !caller.is_empty() => {}
        _ => {
            return Err(ApiError::Forbidden(
                "only the uploader (or the admin, via the admin panel) can delete".into(),
            ));
        }
    }
    remove_share_data(&state, id, &stored_path).await?;
    info!(share_id = id, by = %caller, "share deleted by uploader");
    Ok(StatusCode::NO_CONTENT)
}

/// Shared deletion routine (uploader path here; admin path in `admin_api`).
pub async fn remove_share_data(state: &SharedState, id: u64, stored_path: &str) -> ApiResult<()> {
    let path = PathBuf::from(stored_path);
    // Guard: only ever delete inside the configured share root.
    if let Some(root) = state.share_path() {
        if !path.starts_with(&root) {
            warn!(share_id = id, path = %path.display(), "refusing delete outside share root");
            return Err(ApiError::Internal("share path outside root".into()));
        }
    }
    if path.is_dir() {
        let _ = tokio::fs::remove_dir_all(&path).await;
    } else if path.is_file() {
        let _ = tokio::fs::remove_file(&path).await;
    }
    // Drop the DB row, then cascade-remove any jukebox queue items that pointed
    // at this share so deleting a shared file also clears it from the rotation
    // (otherwise a dangling `shared_file` item lingers and fails when it plays).
    // Lock order matches the jukebox handlers (db → jukebox); both release
    // before the broadcast, which takes the sessions lock.
    let removed_from_queue = {
        let conn = state.db.lock();
        db::delete_share(&conn, id)?;
        state.jukebox.lock().remove_share_refs(&conn, id)?
    };
    if removed_from_queue > 0 {
        info!(
            share_id = id,
            removed_from_queue, "share delete cleared jukebox items"
        );
        state.broadcast_jukebox();
    }
    Ok(())
}
