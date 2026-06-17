//! The client chunk server (TDD §7): when `share_back` is on, a tiny HTTP
//! listener serves `/chunks/{file_id}/{idx}` for chunks present in this
//! client's bitmaps, rate-capped by a token bucket (default 1.5 MB/s, F4.11).
//! Playback machines never run it (HARD CONSTRAINT #8).

use crate::db;
use crate::state::Shared;
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use blt_core::manifest::Manifest;
use blt_core::p2p::TokenBucket;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

struct SeedState {
    app: Shared,
    /// Cached manifests of seeded titles (fetched when first served).
    manifests: Mutex<HashMap<u64, Arc<Manifest>>>,
    bucket: Mutex<(TokenBucket, Instant)>,
}

/// Start the seed listener on an ephemeral port; records it in
/// `ClientState.seed_port`. No-op in playback mode or when share_back is off.
pub async fn start(state: Shared) -> Option<u16> {
    {
        let s = state.settings.read();
        if !s.share_back || s.mode == blt_core::protocol::Mode::Playback {
            return None;
        }
    }
    let cap = state.settings.read().upload_cap_bytes_per_sec;
    let seed = Arc::new(SeedState {
        app: state.clone(),
        manifests: Mutex::new(HashMap::new()),
        bucket: Mutex::new((TokenBucket::new(cap as f64), Instant::now())),
    });
    let router = Router::new()
        .route("/ping", get(|| async { "blt" }))
        .route("/chunks/:file_id/:idx", get(serve_chunk))
        .with_state(seed);

    let listener = match tokio::net::TcpListener::bind("0.0.0.0:0").await {
        Ok(l) => l,
        Err(e) => {
            warn!("chunk server bind failed (seeding disabled): {e}");
            return None;
        }
    };
    let port = listener.local_addr().ok()?.port();
    *state.seed_port.write() = Some(port);
    info!(port, "chunk server (seed) listening");
    tauri::async_runtime::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            warn!("chunk server exited: {e}");
        }
    });
    Some(port)
}

/// Map a `file_id` to `(dest_root, rel_path, offset, size, hash, global_idx)`
/// across all completed/partial downloads this client holds.
async fn serve_chunk(
    State(seed): State<Arc<SeedState>>,
    AxPath((file_id, idx)): AxPath<(u64, u32)>,
) -> impl IntoResponse {
    // Find a download whose manifest contains this file id and whose bitmap has
    // the chunk. Manifests come from the server, cached per title.
    let downloads = {
        let conn = seed.app.db.lock();
        db::list_downloads(&conn).unwrap_or_default()
    };
    let game = seed.app.connection.read().game_endpoint.clone();

    for d in downloads {
        let manifest = {
            let cached = seed.manifests.lock().get(&d.title_id).cloned();
            match cached {
                Some(m) => m,
                None => {
                    let Some(game) = game.as_ref() else { continue };
                    match crate::server_api::title_manifest(game, d.title_id).await {
                        Ok(m) if m.manifest_ver == d.manifest_ver => {
                            let m = Arc::new(m);
                            seed.manifests.lock().insert(d.title_id, m.clone());
                            m
                        }
                        _ => continue,
                    }
                }
            }
        };
        let Some(file) = manifest.file_by_id(file_id) else {
            continue;
        };
        let Some(chunk) = file.chunks.iter().find(|c| c.idx == idx) else {
            continue;
        };
        // Confirm we actually hold the chunk (bitmap bit).
        let global_idx = manifest
            .chunk_locators()
            .iter()
            .find(|l| l.file_id == file_id && l.chunk_idx == idx)
            .map(|l| l.global_idx);
        let have = {
            let conn = seed.app.db.lock();
            match db::load_bitmap(&conn, d.title_id, d.manifest_ver) {
                Ok(Some((bm, _, _))) => global_idx.map(|g| bm.has(g)).unwrap_or(false),
                _ => false,
            }
        };
        if !have {
            return (StatusCode::NOT_FOUND, "chunk not held").into_response();
        }

        // Rate cap (F4.11): token bucket refilled by wall clock; if tokens are
        // short, wait — this throttles our upload without rejecting peers.
        loop {
            let wait = {
                let mut g = seed.bucket.lock();
                let elapsed = g.1.elapsed().as_secs_f64();
                g.1 = Instant::now();
                let (bucket, _) = &mut *g;
                bucket.refill(elapsed);
                if bucket.try_take(chunk.size as f64) {
                    None
                } else {
                    Some(bucket.time_until(chunk.size as f64))
                }
            };
            match wait {
                None => break,
                Some(w) if w.is_finite() => {
                    tokio::time::sleep(std::time::Duration::from_secs_f64(w.min(5.0))).await;
                }
                Some(_) => return (StatusCode::SERVICE_UNAVAILABLE, "cap").into_response(),
            }
        }

        // Read the span from our local copy.
        let path =
            match blt_core::transfer::safe_join(std::path::Path::new(&d.dest_path), &file.rel_path)
            {
                Ok(p) => p,
                Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "path").into_response(),
            };
        let offset = chunk.offset;
        let size = chunk.size as usize;
        let read = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            let mut f = std::fs::File::open(path)?;
            f.seek(SeekFrom::Start(offset))?;
            let mut buf = vec![0u8; size];
            f.read_exact(&mut buf)?;
            Ok(buf)
        })
        .await;
        return match read {
            Ok(Ok(bytes)) => {
                seed.app.seed_meter.lock().add(bytes.len() as u64);
                (StatusCode::OK, bytes).into_response()
            }
            _ => (StatusCode::NOT_FOUND, "unreadable").into_response(),
        };
    }
    (StatusCode::NOT_FOUND, "unknown chunk").into_response()
}
