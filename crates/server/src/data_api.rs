//! Game-distribution service (TDD §5): titles, manifests, file/chunk serving,
//! and the `/ws` live channel (roster, peer registry, jukebox fan-out).
//!
//! Bulk transfer is plain HTTP so it's curl-debuggable and Range-resumable.
//! The SQLite mutex is **never held across an `.await`** — DB lookups complete
//! synchronously, then file IO streams async.

use crate::error::{ApiError, ApiResult};
use crate::state::SharedState;
use crate::util::now_unix;
use crate::{db, library};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use blt_core::protocol::{ClientMsg, ServerMsg, TitleSummary};
use futures_util::{SinkExt, StreamExt};
use std::io::SeekFrom;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::{debug, info, warn};

/// Build the game-distribution router.
pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/titles", get(list_titles))
        .route("/titles/{id}/info", get(title_info))
        .route("/titles/{id}/manifest", get(title_manifest))
        .route("/titles/{id}/script", get(title_script))
        .route("/titles/{id}/files/{file_id}", get(serve_file))
        .route("/chunks/{file_id}/{idx}", get(serve_chunk))
        .route("/admin/verify", axum::routing::post(verify_admin))
        .route("/ws", get(ws_upgrade))
        .with_state(state)
}

/// `POST /admin/verify` — check the admin password without creating a session.
/// DESIGN-NOTE: the playback-lockdown gate (F14.3) reuses the admin password,
/// but the admin *panel* may be bound to a NIC the playback machine can't
/// reach; this endpoint exposes only a yes/no on the game listener. Trusted
/// LAN; equivalent in strength to the login endpoint itself.
async fn verify_admin(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> ApiResult<StatusCode> {
    use argon2::{PasswordHash, PasswordVerifier};
    // This password oracle lives on the no-auth game listener; throttle it the
    // same as /api/login so it can't be brute-forced (SEC-2).
    let ip = addr.ip();
    if state.login_locked(ip) {
        return Err(ApiError::TooManyRequests(
            "too many failed attempts; wait a minute".into(),
        ));
    }
    let password = body
        .get("password")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::BadRequest("password required".into()))?;
    let stored = state
        .config
        .read()
        .admin_password_hash
        .clone()
        .ok_or_else(|| ApiError::Conflict("admin password not set yet".into()))?;
    let parsed =
        PasswordHash::new(&stored).map_err(|e| ApiError::Internal(format!("stored hash: {e}")))?;
    if argon2::Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_err()
    {
        state.login_fail(ip);
        return Err(ApiError::Unauthorized);
    }
    state.login_ok(ip);
    Ok(StatusCode::NO_CONTENT)
}

fn title_to_summary(t: db::TitleRow) -> TitleSummary {
    TitleSummary {
        id: t.id,
        name: t.name,
        label: t.label,
        total_size: t.total_size,
        file_count: t.file_count,
        manifest_ver: t.manifest_ver,
        state: t.state,
        last_scan: t.last_scan,
        info_hash: t.info_hash,
        has_cover: t.has_cover,
        has_install_script: t.has_install_script,
    }
}

/// `GET /titles` — published titles only (a mid-scan title is not advertised,
/// F2.8).
async fn list_titles(State(state): State<SharedState>) -> ApiResult<Json<Vec<TitleSummary>>> {
    let conn = state.db.lock();
    let titles = db::list_titles(&conn)?
        .into_iter()
        .filter(|t| t.state == "published")
        .map(title_to_summary)
        .collect();
    Ok(Json(titles))
}

/// `GET /titles/{id}/info` — the title-info payload (metadata + base64 cover),
/// decoupled from the manifest and cached client-side by `info_hash` (F4.1).
async fn title_info(
    State(state): State<SharedState>,
    AxPath(id): AxPath<u64>,
) -> ApiResult<Response> {
    let json = {
        let conn = state.db.lock();
        db::get_title(&conn, id)?.ok_or(ApiError::NotFound)?;
        db::get_info_json(&conn, id)?
    };
    // No sidecar → empty payload; the client falls back to folder name +
    // placeholder cover (F4.1).
    let body = json.unwrap_or_else(|| "{}".to_string());
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

/// `GET /titles/{id}/manifest` — the full structural manifest (JSON).
async fn title_manifest(
    State(state): State<SharedState>,
    AxPath(id): AxPath<u64>,
) -> ApiResult<Response> {
    let manifest = state.cached_manifest(id).ok_or(ApiError::NotFound)?;
    Ok(Json(manifest.as_ref().clone()).into_response())
}

/// `GET /titles/{id}/script` — Windows post-install script bytes + hash header
/// (TDD §5.2). Canonical-library titles only; 404 when none defined.
async fn title_script(
    State(state): State<SharedState>,
    AxPath(id): AxPath<u64>,
) -> ApiResult<Response> {
    let Some((name, bytes)) = library::read_install_script(&state, id)? else {
        return Err(ApiError::NotFound);
    };
    let hash = blt_core::hash_bytes(&bytes);
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
            (header::HeaderName::from_static("x-blt-hash"), hash.to_hex()),
        ],
        bytes,
    )
        .into_response())
}

/// Resolve a manifest file id to its absolute path under the library.
fn resolve_file(state: &SharedState, file_id: u64) -> ApiResult<(PathBuf, u64)> {
    let (title_folder, rel_path, size) = {
        let conn = state.db.lock();
        db::file_location(&conn, file_id)?.ok_or(ApiError::NotFound)?
    };
    let library = state
        .library_path()
        .ok_or_else(|| ApiError::Internal("library path not configured".into()))?;
    let path = blt_core::transfer::safe_join(&library.join(&title_folder), &rel_path)
        .map_err(|e| ApiError::Internal(format!("bad stored path: {e}")))?;
    Ok((path, size))
}

/// `GET /titles/{id}/files/{file_id}` with Range support (TDD §5) — used for
/// whole-file fetch and HTTP-Range resume. **Streams** from disk: game files
/// are multi-GB and must never be buffered whole in memory.
async fn serve_file(
    State(state): State<SharedState>,
    AxPath((_id, file_id)): AxPath<(u64, u64)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let (path, size) = resolve_file(&state, file_id)?;
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| parse_byte_range(s, size));

    let (start, len, status) = match range {
        Some((start, end)) => (start, end - start + 1, StatusCode::PARTIAL_CONTENT),
        None => (0, size, StatusCode::OK),
    };

    // Delete-race: open allowing concurrent delete so removing the title mid-
    // stream doesn't leave the file pinned on Windows (crate::util::open_read_shared).
    let mut f = tokio::fs::File::from_std(crate::util::open_read_shared(&path)?);
    f.seek(SeekFrom::Start(start)).await?;
    let stream = tokio_util::io::ReaderStream::with_capacity(f.take(len), 256 * 1024);
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

/// `GET /chunks/{file_id}/{idx}` — one chunk (the server seed path, TDD §5).
async fn serve_chunk(
    State(state): State<SharedState>,
    AxPath((file_id, idx)): AxPath<(u64, u32)>,
) -> ApiResult<Response> {
    let (title_folder, rel_path, offset, size) = {
        let conn = state.db.lock();
        db::chunk_location(&conn, file_id, idx)?.ok_or(ApiError::NotFound)?
    };
    let library = state
        .library_path()
        .ok_or_else(|| ApiError::Internal("library path not configured".into()))?;
    let path = blt_core::transfer::safe_join(&library.join(&title_folder), &rel_path)
        .map_err(|e| ApiError::Internal(format!("bad stored path: {e}")))?;
    let body = read_span(&path, offset, size).await?;
    state.serve_meter.lock().add(body.len() as u64);
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, size.to_string()),
        ],
        body,
    )
        .into_response())
}

/// Read `len` bytes at `offset` (async). Chunks are ≤ 4 MiB by design so a
/// buffered read per request is fine.
async fn read_span(path: &std::path::Path, offset: u64, len: u64) -> ApiResult<Vec<u8>> {
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(SeekFrom::Start(offset)).await?;
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf)
        .await
        .map_err(|e| ApiError::Internal(format!("short read on {}: {e}", path.display())))?;
    Ok(buf)
}

/// Parse `bytes=a-b` / `bytes=a-` (single range only). Returns inclusive
/// `(start, end)` clamped to `size`, or `None` for anything unparseable.
pub(crate) fn parse_byte_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let spec = value.strip_prefix("bytes=")?;
    if spec.contains(',') || size == 0 {
        return None; // multi-range unsupported
    }
    let (a, b) = spec.split_once('-')?;
    match (a.is_empty(), b.is_empty()) {
        (false, false) => {
            let start: u64 = a.parse().ok()?;
            let end: u64 = b.parse().ok()?;
            (start <= end && start < size).then(|| (start, end.min(size - 1)))
        }
        (false, true) => {
            let start: u64 = a.parse().ok()?;
            (start < size).then(|| (start, size - 1))
        }
        (true, false) => {
            // suffix range: last N bytes
            let n: u64 = b.parse().ok()?;
            let n = n.min(size);
            (n > 0).then(|| (size - n, size - 1))
        }
        (true, true) => None,
    }
}

// ───────────────────────────── /ws ─────────────────────────────────

async fn ws_upgrade(
    State(state): State<SharedState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| ws_session(state, socket, remote))
}

/// One websocket session: registry entry + read loop + write pump.
async fn ws_session(state: SharedState, socket: WebSocket, remote: SocketAddr) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ServerMsg>();

    // Until Hello arrives the session is anonymous.
    let conn_id = state
        .sessions
        .lock()
        .add(format!("@anon-{remote}"), tx.clone());

    // Write pump: registry → socket.
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let text = match serde_json::to_string(&msg) {
                Ok(t) => t,
                Err(e) => {
                    // A wire type that cannot serialise is a protocol bug —
                    // never drop it silently (see ServerMsg::Roster history).
                    tracing::error!("ws message failed to serialise: {e}");
                    continue;
                }
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let (server_uuid, server_label) = {
        let cfg = state.config.read();
        (cfg.server_uuid.clone(), cfg.server_label.clone())
    };
    let _ = tx.send(ServerMsg::Welcome {
        server_uuid,
        server_label,
    });

    let mut client_id = String::new();
    loop {
        // Drop the session if the client goes silent (an abrupt drop / sleep sends
        // no Close frame). A healthy client pings every 20s, so 90s only trips on a
        // genuinely dead peer — frees the fd and removes it from the roster instead
        // of leaking a zombie session.
        let msg =
            match tokio::time::timeout(std::time::Duration::from_secs(90), stream.next()).await {
                Ok(Some(Ok(m))) => m,
                _ => break, // timeout, stream end, or ws error
            };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(parsed) = serde_json::from_str::<ClientMsg>(&text) else {
            debug!(%remote, "unparseable ws message");
            continue;
        };
        handle_client_msg(&state, conn_id, &mut client_id, remote, parsed, &tx);
    }

    // Disconnect: drop registry entry + tell everyone (F15.3 roster removal).
    // Snapshot whether this was the *last* playback machine before removing, so
    // we can reset the jukebox if it leaves mid-playback (below).
    let last_playback_gone = {
        let mut sessions = state.sessions.lock();
        let was_playback = sessions
            .conn_info(conn_id)
            .map(|(_, _, mode)| mode == blt_core::protocol::Mode::Playback)
            .unwrap_or(false);
        sessions.remove(conn_id);
        was_playback && sessions.playback_count() == 0
    };
    state.broadcast_roster();

    // The playback machine just dropped while something was playing → nothing is
    // actually rendering. Clear the phantom now-playing and requeue it on top so
    // the jukebox doesn't show a stale "now playing" (#12) and resumes cleanly
    // when a playback machine returns.
    if last_playback_gone {
        let changed = {
            let conn = state.db.lock();
            let mut jb = state.jukebox.lock();
            match jb.requeue_now_playing(&conn) {
                Ok(c) => c,
                Err(e) => {
                    warn!("requeue on playback disconnect: {e}");
                    false
                }
            }
        };
        if changed {
            info!("playback machine gone mid-play — now-playing requeued to top, jukebox idle");
            state.broadcast_jukebox();
        }
    }

    writer.abort();
    debug!(%remote, "ws session closed");
}

/// Handle one client→server message. Synchronous: locks are taken and released
/// without any `.await` in scope.
fn handle_client_msg(
    state: &SharedState,
    conn_id: u64,
    client_id: &mut String,
    remote: SocketAddr,
    msg: ClientMsg,
    tx: &tokio::sync::mpsc::UnboundedSender<ServerMsg>,
) {
    match msg {
        ClientMsg::Hello {
            client_id: cid,
            display_name,
            machine_name,
            mode,
        } => {
            *client_id = cid.clone();
            {
                let mut sessions = state.sessions.lock();
                sessions.update_hello(
                    conn_id,
                    cid.clone(),
                    display_name.clone(),
                    machine_name,
                    mode,
                );
            }
            // Persist the display name (durable bit, F7.2).
            {
                let conn = state.db.lock();
                if let Err(e) = db::set_name(&conn, &cid, &display_name, now_unix()) {
                    warn!("persist name: {e}");
                }
            }
            info!(%cid, name = %display_name, %remote, "client hello");
            state.broadcast_roster();
            // Reachability self-test bootstrap (F13.4): hand 1–2 peer addresses.
            let probes = {
                let sessions = state.sessions.lock();
                sessions.probe_candidates(&cid, 2)
            };
            let _ = tx.send(ServerMsg::ProbePeers { peers: probes });
            // Initial state push (jukebox snapshot for this viewer).
            send_jukebox_to(state, &cid, tx);
        }
        ClientMsg::Announce {
            title_id,
            manifest_ver,
            chunk_endpoint,
        } => {
            let endpoint = normalize_endpoint(&chunk_endpoint, remote);
            let mut sessions = state.sessions.lock();
            // HARD CONSTRAINT #8: a dedicated playback client never downloads
            // games — and never seeds them either. Refuse registry entries
            // from playback-mode sessions server-side.
            let is_playback = sessions
                .conn_info(conn_id)
                .map(|(_, _, mode)| mode == blt_core::protocol::Mode::Playback)
                .unwrap_or(false);
            if is_playback {
                warn!(%client_id, "ignoring peer announce from playback-mode session");
            } else {
                sessions.announce_peer(client_id, title_id, manifest_ver, endpoint);
            }
        }
        ClientMsg::Unannounce {
            title_id,
            manifest_ver,
        } => {
            let mut sessions = state.sessions.lock();
            sessions.unannounce_peer(client_id, title_id, manifest_ver);
        }
        ClientMsg::Activity {
            activity,
            throughput_bps,
            server_only,
        } => {
            {
                let mut sessions = state.sessions.lock();
                sessions.update_activity(conn_id, activity, throughput_bps, server_only);
            }
            state.broadcast_roster();
        }
        ClientMsg::RequestPeers {
            title_id,
            manifest_ver,
        } => {
            let peers = {
                let sessions = state.sessions.lock();
                sessions.peers_for(title_id, manifest_ver, client_id)
            };
            let _ = tx.send(ServerMsg::Peers {
                title_id,
                manifest_ver,
                peers,
            });
        }
        ClientMsg::ReachabilityResult { peer, reachable } => {
            debug!(%client_id, %peer, reachable, "reachability probe result");
        }
        ClientMsg::Resync => {
            // Full state resync after reconnect (HARD CONSTRAINT #12 / F15.1).
            send_jukebox_to(state, client_id, tx);
            let roster = state.sessions.lock().roster();
            let _ = tx.send(ServerMsg::Roster { roster });
        }
        ClientMsg::JukeboxAdd {
            item_type,
            reference,
            title,
        } => {
            // Attribution from the session (F8.1/F8.3); display name may still
            // be empty if Hello never arrived — fall back to the client id.
            let (cid, name, _mode) = {
                let sessions = state.sessions.lock();
                match sessions.conn_info(conn_id) {
                    Some(info) => info,
                    None => return,
                }
            };
            let display_name = if name.is_empty() { cid.clone() } else { name };
            if !crate::jukebox::Jukebox::ref_acceptable(item_type, &reference) {
                warn!(ty = ?item_type, "rejected non-http(s) jukebox ref");
                return;
            }
            let res = {
                let conn = state.db.lock();
                let jb = state.jukebox.lock();
                jb.add(
                    &conn,
                    item_type,
                    &reference,
                    title.as_deref(),
                    &display_name,
                    &cid,
                    now_unix(),
                )
                .inspect(|&id| {
                    // The submitter auto-upvotes their own pick (keyed on
                    // client_id, #13).
                    let _ = jb.toggle_vote(&conn, id, &cid);
                })
            };
            match res {
                Ok(id) => {
                    info!(item = id, by = %display_name, ty = ?item_type, "jukebox add");
                    state.broadcast_jukebox();
                }
                Err(e) => warn!("jukebox add: {e}"),
            }
        }
        ClientMsg::JukeboxVote { item_id } => {
            let res = {
                let conn = state.db.lock();
                let jb = state.jukebox.lock();
                jb.toggle_vote(&conn, item_id, client_id)
            };
            match res {
                Ok(_) => state.broadcast_jukebox(),
                Err(e) => warn!("jukebox vote: {e}"),
            }
        }
        ClientMsg::JukeboxNext | ClientMsg::JukeboxEnded => {
            // Next/ended only from the playback machine (clients have no Next,
            // F8.7/F10.4; the admin panel uses its REST route).
            let is_playback = {
                let sessions = state.sessions.lock();
                sessions
                    .conn_info(conn_id)
                    .map(|(_, _, mode)| mode == blt_core::protocol::Mode::Playback)
                    .unwrap_or(false)
            };
            if !is_playback {
                warn!(%client_id, "ignoring Next/Ended from non-playback session");
                return;
            }
            let res = {
                let conn = state.db.lock();
                let mut jb = state.jukebox.lock();
                jb.advance(&conn)
            };
            match res {
                Ok(()) => state.broadcast_jukebox(),
                Err(e) => warn!("jukebox advance: {e}"),
            }
        }
        ClientMsg::Ping => {
            let _ = tx.send(ServerMsg::Pong);
        }
    }
}

fn send_jukebox_to(
    state: &SharedState,
    viewer: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<ServerMsg>,
) {
    let conn = state.db.lock();
    let jb = state.jukebox.lock();
    if let Ok(snapshot) = jb.state_for(&conn, viewer) {
        let _ = tx.send(ServerMsg::Jukebox(snapshot));
    }
}

/// A peer announcing `0.0.0.0:port` (or an empty host) gets its observed remote
/// IP substituted so other clients receive a reachable address.
fn normalize_endpoint(announced: &str, remote: SocketAddr) -> String {
    // Always use the observed remote IP; only the announced PORT is trusted, so
    // a client can't advertise an arbitrary host (SEC-8). A missing/invalid port
    // (e.g. one with an embedded path) falls back to the connection's port.
    let port: u16 = announced
        .rsplit_once(':')
        .and_then(|(_, p)| p.trim().parse().ok())
        .unwrap_or_else(|| remote.port());
    format!("{}:{port}", remote.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_range_parsing() {
        assert_eq!(parse_byte_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_byte_range("bytes=100-", 1000), Some((100, 999)));
        assert_eq!(parse_byte_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_byte_range("bytes=0-5000", 1000), Some((0, 999)));
        assert_eq!(parse_byte_range("bytes=1000-", 1000), None); // past EOF
        assert_eq!(parse_byte_range("bytes=5-2", 1000), None);
        assert_eq!(parse_byte_range("bytes=0-1,5-9", 1000), None); // multi
        assert_eq!(parse_byte_range("nonsense", 1000), None);
        assert_eq!(parse_byte_range("bytes=0-0", 0), None);
    }

    #[test]
    fn endpoint_normalisation() {
        let remote: SocketAddr = "192.168.1.42:55555".parse().unwrap();
        assert_eq!(
            normalize_endpoint("0.0.0.0:9000", remote),
            "192.168.1.42:9000"
        );
        assert_eq!(normalize_endpoint(":9000", remote), "192.168.1.42:9000");
        // Announced host is ignored — always the observed remote IP (SEC-8).
        assert_eq!(
            normalize_endpoint("10.0.0.5:9000", remote),
            "192.168.1.42:9000"
        );
        // Embedded path / bad port → falls back to the connection's port.
        assert_eq!(
            normalize_endpoint("evil:80/x", remote),
            "192.168.1.42:55555"
        );
    }
}
