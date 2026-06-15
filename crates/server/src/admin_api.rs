//! Admin panel API (password-gated, F1.6/F11) + static SPA serving.
//!
//! The **only authenticated surface** (TDD §11): a single shared password,
//! argon2-hashed in `config.toml`, session cookie in an in-memory set. The
//! password is set on first run via `/api/setup` and changeable thereafter.
//! Every `/api/*` route except `setup`/`login`/`auth-state` requires a session.
//! **Never log the password** (HARD CONSTRAINT #16).

use crate::error::{ApiError, ApiResult};
use crate::library;
use crate::state::{ServiceKind, SharedState};
use crate::{bindings, db, share};
use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use axum::extract::{ConnectInfo, Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use blt_core::jukebox::OrderMode;
use blt_core::protocol::ItemType;
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use tracing::{info, warn};

const COOKIE_NAME: &str = "blt_admin";
const MIN_PASSWORD_LEN: usize = 8;

/// Build the admin router: `/api/*` + the SPA static mount.
pub fn router(state: SharedState) -> Router {
    let open = Router::new()
        .route("/api/setup", post(setup))
        .route("/api/login", post(login))
        .route("/api/auth-state", get(auth_state));

    let gated = Router::new()
        .route("/api/logout", post(logout))
        .route("/api/password", put(change_password))
        .route("/api/status", get(status))
        .route("/api/interfaces", get(interfaces))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/scan", post(scan_now))
        .route("/api/services/:kind/restart", post(restart_service))
        .route("/api/server/restart", post(restart_server))
        .route("/api/fs", get(fs_browse))
        .route("/api/update/check", get(update_check))
        .route("/api/update/install", post(update_install))
        .route("/api/titles", get(titles))
        .route("/api/titles/:id/label", put(set_label))
        .route("/api/shares", get(admin_shares))
        .route("/api/shares/:id", axum::routing::delete(admin_delete_share))
        .route("/api/jukebox", get(jukebox_state))
        .route("/api/jukebox/items", post(jukebox_add))
        .route(
            "/api/jukebox/items/:id",
            axum::routing::delete(jukebox_remove),
        )
        .route("/api/jukebox/items/:id/top", post(jukebox_top))
        .route("/api/jukebox/next", post(jukebox_next))
        .route("/api/jukebox/clear", post(jukebox_clear))
        .route("/api/jukebox/mode", put(jukebox_mode))
        .route("/api/log", get(log_tail))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    // SPA: served from disk; path overridable for dev (BLT_ADMIN_WEB).
    // DESIGN-NOTE: v1 serves the admin SPA from a directory next to the binary
    // (or $BLT_ADMIN_WEB); M8 packaging copies admin-web/dist there. Embedding
    // into the binary (rust-embed) is a drop-in later change.
    let spa_dir = std::env::var("BLT_ADMIN_WEB").unwrap_or_else(|_| {
        std::env::current_exe()
            .ok()
            .and_then(|p| {
                p.parent()
                    .map(|d| d.join("admin-web").to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "admin-web/dist".to_string())
    });
    let static_svc = tower_http::services::ServeDir::new(&spa_dir).fallback(
        tower_http::services::ServeFile::new(format!("{spa_dir}/index.html")),
    );

    open.merge(gated)
        .fallback_service(static_svc)
        .with_state(state)
}

// ───────────────────────────── auth ────────────────────────────────

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == COOKIE_NAME).then(|| v.to_string())
    })
}

async fn require_auth(
    State(state): State<SharedState>,
    headers: HeaderMap,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    // CSRF defence-in-depth (SEC-4): for state-changing methods, reject when an
    // Origin/Referer is present and its host doesn't match our Host. This
    // survives future edits that might weaken the SameSite=Strict cookie or add
    // CORS. Same-origin requests (the SPA) send a matching Origin or none.
    let m = req.method();
    let safe_method = *m == Method::GET || *m == Method::HEAD || *m == Method::OPTIONS;
    if !safe_method && !same_origin(&headers) {
        return ApiError::Forbidden("cross-origin request rejected".into()).into_response();
    }
    let ok = cookie_token(&headers)
        .map(|t| state.session_valid(&t))
        .unwrap_or(false);
    if !ok {
        return ApiError::Unauthorized.into_response();
    }
    next.run(req).await
}

/// True if there is no cross-origin signal, or the Origin/Referer host matches
/// the request's Host (SEC-4).
fn same_origin(headers: &HeaderMap) -> bool {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let claimed = headers
        .get(header::ORIGIN)
        .or_else(|| headers.get(header::REFERER))
        .and_then(|v| v.to_str().ok());
    match (host, claimed) {
        // No Origin/Referer → not a browser cross-site form/XHR; allow.
        (_, None) => true,
        (Some(host), Some(claimed)) => claimed
            .split("://")
            .nth(1)
            .map(|rest| rest.split('/').next().unwrap_or("") == host)
            .unwrap_or(false),
        _ => false,
    }
}

#[derive(Deserialize)]
struct PasswordBody {
    password: String,
}

/// First-run password set (F1.6). Refused once a hash exists.
async fn setup(
    State(state): State<SharedState>,
    Json(body): Json<PasswordBody>,
) -> ApiResult<Response> {
    if body.password.len() < MIN_PASSWORD_LEN {
        return Err(ApiError::BadRequest(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    // Hash before taking the lock (argon2 is slow); keep the check-then-write
    // under a single write lock so concurrent first-run setups can't race (SEC-9).
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(body.password.as_bytes(), &salt)
        .map_err(|e| ApiError::Internal(format!("hash: {e}")))?
        .to_string();
    {
        let mut cfg = state.config.write();
        if cfg.admin_password_hash.is_some() {
            return Err(ApiError::Conflict("password already set".into()));
        }
        cfg.admin_password_hash = Some(hash);
        cfg.save(&state.config_path)
            .map_err(|e| ApiError::Internal(format!("save config: {e}")))?;
    }
    info!("admin password set (first run)");
    issue_session(&state)
}

async fn login(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<PasswordBody>,
) -> ApiResult<Response> {
    let ip = addr.ip();
    if state.login_locked(ip) {
        return Err(ApiError::TooManyRequests(
            "too many failed attempts; wait a minute".into(),
        ));
    }
    let stored = {
        let cfg = state.config.read();
        cfg.admin_password_hash.clone()
    };
    let Some(stored) = stored else {
        return Err(ApiError::Conflict(
            "password not set yet (use setup)".into(),
        ));
    };
    let parsed =
        PasswordHash::new(&stored).map_err(|e| ApiError::Internal(format!("stored hash: {e}")))?;
    if Argon2::default()
        .verify_password(body.password.as_bytes(), &parsed)
        .is_err()
    {
        state.login_fail(ip);
        warn!(%ip, "admin login failed");
        return Err(ApiError::Unauthorized);
    }
    state.login_ok(ip);
    info!("admin logged in");
    issue_session(&state)
}

fn issue_session(state: &SharedState) -> ApiResult<Response> {
    let token = state.session_new();
    Ok((
        [(
            header::SET_COOKIE,
            format!("{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict"),
        )],
        Json(json!({"ok": true})),
    )
        .into_response())
}

/// Whether setup is needed / a session is valid — drives the SPA's first screen.
async fn auth_state(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let needs_setup = state.config.read().admin_password_hash.is_none();
    let authed = cookie_token(&headers)
        .map(|t| state.session_valid(&t))
        .unwrap_or(false);
    Json(json!({"needs_setup": needs_setup, "authed": authed}))
}

async fn logout(State(state): State<SharedState>, headers: HeaderMap) -> Json<serde_json::Value> {
    if let Some(t) = cookie_token(&headers) {
        state.session_remove(&t);
    }
    Json(json!({"ok": true}))
}

#[derive(Deserialize)]
struct ChangePassword {
    current: String,
    new: String,
}

async fn change_password(
    State(state): State<SharedState>,
    Json(body): Json<ChangePassword>,
) -> ApiResult<Response> {
    let stored = state
        .config
        .read()
        .admin_password_hash
        .clone()
        .ok_or_else(|| ApiError::Conflict("password not set".into()))?;
    let parsed =
        PasswordHash::new(&stored).map_err(|e| ApiError::Internal(format!("stored hash: {e}")))?;
    if Argon2::default()
        .verify_password(body.current.as_bytes(), &parsed)
        .is_err()
    {
        return Err(ApiError::Unauthorized);
    }
    if body.new.len() < MIN_PASSWORD_LEN {
        return Err(ApiError::BadRequest(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(body.new.as_bytes(), &salt)
        .map_err(|e| ApiError::Internal(format!("hash: {e}")))?
        .to_string();
    {
        let mut cfg = state.config.write();
        cfg.admin_password_hash = Some(hash);
        cfg.save(&state.config_path)
            .map_err(|e| ApiError::Internal(format!("save config: {e}")))?;
    }
    // Revoke every existing session, then hand the caller a fresh one (SEC-5).
    state.session_clear();
    info!("admin password changed; all sessions revoked");
    issue_session(&state)
}

// ─────────────────────────── status/config ─────────────────────────

async fn status(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let cfg = state.config.read();
    let conns = state.sessions.lock().conn_count();
    Json(json!({
        "uuid": cfg.server_uuid,
        "label": cfg.server_label,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": state.started.elapsed().as_secs(),
        "connections": conns,
        "binds": {
            "game_distribution": cfg.game_distribution_bind,
            "shared_pool": cfg.shared_pool_bind,
            "admin_panel": cfg.admin_panel_bind,
        },
        "paths": {
            "library": cfg.library_path,
            "staging": cfg.staging_path,
            "share": cfg.share_path,
        },
    }))
}

async fn interfaces() -> Json<Vec<bindings::InterfaceInfo>> {
    Json(bindings::list_interfaces())
}

async fn get_config(State(state): State<SharedState>) -> Json<serde_json::Value> {
    let cfg = state.config.read();
    // Everything except the password hash (never expose it).
    Json(json!({
        "game_distribution_bind": cfg.game_distribution_bind,
        "shared_pool_bind": cfg.shared_pool_bind,
        "admin_panel_bind": cfg.admin_panel_bind,
        "library_path": cfg.library_path,
        "staging_path": cfg.staging_path,
        "share_path": cfg.share_path,
        "chunk_size": cfg.chunk_size,
        "staging_settle_secs": cfg.staging_settle_secs,
        "scan_interval_secs": cfg.scan_interval_secs,
        "db_backup_interval_secs": cfg.db_backup_interval_secs,
        "peer_timeout_secs": cfg.peer_timeout_secs,
        "jukebox_order_mode": cfg.jukebox_order_mode,
        "server_label": cfg.server_label,
    }))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ConfigPatch {
    game_distribution_bind: Option<String>,
    shared_pool_bind: Option<String>,
    admin_panel_bind: Option<String>,
    library_path: Option<String>,
    staging_path: Option<String>,
    share_path: Option<String>,
    chunk_size: Option<u64>,
    staging_settle_secs: Option<u64>,
    scan_interval_secs: Option<u64>,
    db_backup_interval_secs: Option<u64>,
    peer_timeout_secs: Option<u64>,
    server_label: Option<String>,
    // Accepted (and ignored) so the SPA's full-object round-trip validates:
    // GET /api/config emits this, the Settings page PUTs the whole object back,
    // and `deny_unknown_fields` would otherwise 422. The jukebox order mode is
    // owned by PUT /api/jukebox/mode, not the Settings page. (BUG-1)
    jukebox_order_mode: Option<String>,
}

/// `PUT /api/config` — persist (config.toml + SQLite mirror) and rebind only
/// the affected listeners (F1.3).
async fn put_config(
    State(state): State<SharedState>,
    Json(patch): Json<ConfigPatch>,
) -> ApiResult<Json<serde_json::Value>> {
    // Validate new binds parse before committing anything.
    for (label, bind) in [
        ("game_distribution_bind", &patch.game_distribution_bind),
        ("shared_pool_bind", &patch.shared_pool_bind),
        ("admin_panel_bind", &patch.admin_panel_bind),
    ] {
        if let Some(b) = bind {
            b.parse::<std::net::SocketAddr>()
                .map_err(|e| ApiError::BadRequest(format!("{label} {b:?}: {e}")))?;
        }
    }

    let mut rebinds: Vec<ServiceKind> = Vec::new();
    // Honour an order-mode round-tripped from the Settings page (normally
    // unchanged; the Jukebox page is the real editor). Applied after the config
    // lock to avoid holding it across the jukebox/broadcast locks. (BUG-1)
    let mut new_order_mode: Option<OrderMode> = None;
    {
        let mut cfg = state.config.write();
        if let Some(v) = patch.jukebox_order_mode {
            let mode = OrderMode::from_str_lossy(&v);
            if mode.as_str() != cfg.jukebox_order_mode {
                cfg.jukebox_order_mode = mode.as_str().to_string();
                new_order_mode = Some(mode);
            }
        }
        if let Some(v) = patch.game_distribution_bind {
            if v != cfg.game_distribution_bind {
                cfg.game_distribution_bind = v;
                rebinds.push(ServiceKind::Game);
            }
        }
        if let Some(v) = patch.shared_pool_bind {
            if v != cfg.shared_pool_bind {
                cfg.shared_pool_bind = v;
                rebinds.push(ServiceKind::Share);
            }
        }
        if let Some(v) = patch.admin_panel_bind {
            if v != cfg.admin_panel_bind {
                cfg.admin_panel_bind = v;
                rebinds.push(ServiceKind::Admin);
            }
        }
        if let Some(v) = patch.library_path {
            cfg.library_path = (!v.is_empty()).then(|| v.into());
        }
        if let Some(v) = patch.staging_path {
            cfg.staging_path = (!v.is_empty()).then(|| v.into());
        }
        if let Some(v) = patch.share_path {
            cfg.share_path = (!v.is_empty()).then(|| v.into());
        }
        if let Some(v) = patch.chunk_size {
            cfg.chunk_size = v.max(64 * 1024);
        }
        if let Some(v) = patch.staging_settle_secs {
            cfg.staging_settle_secs = v;
        }
        if let Some(v) = patch.scan_interval_secs {
            cfg.scan_interval_secs = v;
        }
        if let Some(v) = patch.db_backup_interval_secs {
            cfg.db_backup_interval_secs = v;
        }
        if let Some(v) = patch.peer_timeout_secs {
            cfg.peer_timeout_secs = v;
        }
        if let Some(v) = patch.server_label {
            cfg.server_label = v;
        }
        cfg.save(&state.config_path)
            .map_err(|e| ApiError::Internal(format!("save config: {e}")))?;

        // Mirror big-data paths into live state + SQLite (F1.3 dual persistence).
        let mut paths = state.paths.write();
        paths.library = cfg.library_path.clone();
        paths.staging = cfg.staging_path.clone();
        paths.share = cfg.share_path.clone();
        let conn = state.db.lock();
        for (k, v) in [
            ("game_distribution_bind", cfg.game_distribution_bind.clone()),
            ("shared_pool_bind", cfg.shared_pool_bind.clone()),
            ("admin_panel_bind", cfg.admin_panel_bind.clone()),
            ("chunk_size", cfg.chunk_size.to_string()),
            ("scan_interval_secs", cfg.scan_interval_secs.to_string()),
        ] {
            let _ = db::set_config(&conn, k, &v);
        }
    }

    if let Some(mode) = new_order_mode {
        state.jukebox.lock().set_mode(mode);
        state.broadcast_jukebox();
    }

    for kind in &rebinds {
        if let Some(tx) = state.rebind.get() {
            let _ = tx.send(*kind);
        }
    }
    info!(?rebinds, "config updated");
    Ok(Json(json!({"ok": true, "rebinding": !rebinds.is_empty()})))
}

// ──────────────────────────── service control ──────────────────────

/// `POST /api/services/:kind/restart` — safely restart one listener
/// (`game`|`share`|`admin`) by re-triggering the supervisor's rebind path:
/// graceful shutdown (in-flight requests drain) then a fresh bind on the same
/// address. Clients auto-reconnect (HARD CONSTRAINT #12); downloads resume.
async fn restart_service(
    State(state): State<SharedState>,
    AxPath(kind): AxPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let svc = match kind.as_str() {
        "game" => ServiceKind::Game,
        "share" => ServiceKind::Share,
        "admin" => ServiceKind::Admin,
        other => return Err(ApiError::BadRequest(format!("unknown service: {other}"))),
    };
    match state.rebind.get() {
        Some(tx) => {
            tx.send(svc)
                .map_err(|_| ApiError::Internal("rebind channel closed".into()))?;
            info!(?svc, "service restart requested via admin panel");
            Ok(Json(json!({ "ok": true, "service": kind })))
        }
        None => Err(ApiError::Internal("supervisor not ready".into())),
    }
}

/// `POST /api/server/restart` — bounce the whole process. Responds first, then
/// checkpoints the DB and re-execs the binary (see `crate::reexec`). Disruptive
/// → the SPA gates it behind a confirm (HARD CONSTRAINT #5); never automatic
/// (HARD CONSTRAINT #2).
async fn restart_server(State(state): State<SharedState>) -> ApiResult<Json<serde_json::Value>> {
    info!("full server restart requested via admin panel");
    {
        // Best-effort WAL checkpoint so the fresh process starts from a clean
        // main DB file (#13 keeps WAL mode; this just truncates the log).
        let conn = state.db.lock();
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
    // Defer so this 200 flushes to the admin before the image is replaced.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        crate::reexec();
    });
    Ok(Json(json!({ "ok": true, "restarting": true })))
}

// ────────────────────────── filesystem browse ──────────────────────

#[derive(Deserialize)]
struct FsQuery {
    #[serde(default)]
    path: String,
}

/// `GET /api/fs?path=…` — list the sub-directories of a server-side folder so
/// the admin panel can offer a "Browse…" picker for the storage paths (ENH-1).
/// Admin-gated (the admin legitimately browses the server's own disk). Empty
/// path → the server's home directory.
async fn fs_browse(Query(q): Query<FsQuery>) -> ApiResult<Json<serde_json::Value>> {
    let start = if q.path.trim().is_empty() {
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| "/".to_string())
    } else {
        q.path.clone()
    };
    // Resolve symlinks/.. up front; refuse if it doesn't resolve (SEC-6 — no
    // silent fall-through to an unresolved path).
    let canon = std::path::Path::new(&start)
        .canonicalize()
        .map_err(|e| ApiError::BadRequest(format!("cannot resolve {start:?}: {e}")))?;
    if !canon.is_dir() {
        return Err(ApiError::BadRequest(format!(
            "not a directory: {}",
            canon.display()
        )));
    }
    // Cap entries so a huge directory can't spike memory/CPU (SEC-6).
    const MAX_ENTRIES: usize = 2000;
    let mut dirs: Vec<serde_json::Value> = Vec::new();
    let mut truncated = false;
    let rd = std::fs::read_dir(&canon)
        .map_err(|e| ApiError::BadRequest(format!("cannot read {}: {e}", canon.display())))?;
    for entry in rd.flatten() {
        if dirs.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') {
                continue; // hide dotfolders to reduce noise
            }
            dirs.push(json!({ "name": name, "path": p.to_string_lossy() }));
        }
    }
    dirs.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });
    Ok(Json(json!({
        "path": canon.to_string_lossy(),
        "parent": canon.parent().map(|p| p.to_string_lossy().to_string()),
        "dirs": dirs,
        "truncated": truncated,
    })))
}

// ───────────────────────────── self-update ─────────────────────────

/// `GET /api/update/check` — is a newer server release published? (ENH-2)
async fn update_check() -> ApiResult<Json<crate::update::UpdateInfo>> {
    let info = crate::update::check()
        .await
        .map_err(|e| ApiError::Internal(format!("update check: {e}")))?;
    Ok(Json(info))
}

/// `POST /api/update/install` — download the latest server tarball, replace the
/// install in place, then re-exec. Manual + confirmed only (HARD CONSTRAINT #2);
/// the SPA gates it. Errors out cleanly (nothing replaced) on any failure
/// before the commit point.
async fn update_install(State(state): State<SharedState>) -> ApiResult<Json<serde_json::Value>> {
    crate::update::install()
        .await
        .map_err(|e| ApiError::BadRequest(format!("update: {e}")))?;
    info!("server update installed; re-executing");
    {
        let conn = state.db.lock();
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        crate::reexec();
    });
    Ok(Json(json!({ "ok": true, "restarting": true })))
}

// ───────────────────────────── library ─────────────────────────────

/// `POST /api/scan` — manual "Scan now" (F2.5). Runs blocking work off the
/// async pool.
async fn scan_now(State(state): State<SharedState>) -> ApiResult<Json<library::ScanSummary>> {
    let st = state.clone();
    let summary = tokio::task::spawn_blocking(move || library::scan_library(&st))
        .await
        .map_err(|e| ApiError::Internal(format!("scan task: {e}")))??;
    Ok(Json(summary))
}

/// `GET /api/titles` — full listing for the panel incl. scanning state (F2.7).
async fn titles(State(state): State<SharedState>) -> ApiResult<Json<serde_json::Value>> {
    let conn = state.db.lock();
    let rows = db::list_titles(&conn)?;
    let out: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|t| {
            json!({
                "id": t.id,
                "name": t.name,
                "label": t.label,
                "manifest_ver": t.manifest_ver,
                "total_size": t.total_size,
                "file_count": t.file_count,
                "state": t.state,
                "last_scan": t.last_scan,
                "has_metadata": t.info_hash.is_some(),
                "has_cover": t.has_cover,
                "has_install_script": t.has_install_script,
            })
        })
        .collect();
    Ok(Json(json!(out)))
}

#[derive(Deserialize)]
struct LabelBody {
    label: Option<String>,
}

async fn set_label(
    State(state): State<SharedState>,
    AxPath(id): AxPath<u64>,
    Json(body): Json<LabelBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let conn = state.db.lock();
    db::get_title(&conn, id)?.ok_or(ApiError::NotFound)?;
    db::set_title_label(&conn, id, body.label.as_deref())?;
    Ok(Json(json!({"ok": true})))
}

// ───────────────────────────── shares ──────────────────────────────

async fn admin_shares(
    State(state): State<SharedState>,
) -> ApiResult<Json<Vec<blt_core::protocol::ShareSummary>>> {
    let conn = state.db.lock();
    Ok(Json(db::list_shares(&conn)?))
}

/// Admin can delete **any** share (F6.9 / F11.2) — this listener is
/// password-gated so no ownership check.
async fn admin_delete_share(
    State(state): State<SharedState>,
    AxPath(id): AxPath<u64>,
) -> ApiResult<StatusCode> {
    let stored = {
        let conn = state.db.lock();
        db::share_stored_path(&conn, id)?
    };
    let Some((stored_path, _)) = stored else {
        return Err(ApiError::NotFound);
    };
    share::remove_share_data(&state, id, &stored_path).await?;
    info!(share_id = id, "share deleted by admin");
    Ok(StatusCode::NO_CONTENT)
}

// ───────────────────────────── jukebox ─────────────────────────────

async fn jukebox_state(
    State(state): State<SharedState>,
) -> ApiResult<Json<blt_core::protocol::JukeboxState>> {
    let conn = state.db.lock();
    let jb = state.jukebox.lock();
    Ok(Json(jb.state_for(&conn, "@admin")?))
}

#[derive(Deserialize)]
struct AddItem {
    #[serde(rename = "type")]
    item_type: String,
    #[serde(rename = "ref")]
    reference: String,
    title: Option<String>,
}

async fn jukebox_add(
    State(state): State<SharedState>,
    Json(body): Json<AddItem>,
) -> ApiResult<Json<serde_json::Value>> {
    let ty = ItemType::from_str_opt(&body.item_type)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown item type {:?}", body.item_type)))?;
    if !crate::jukebox::Jukebox::ref_acceptable(ty, &body.reference) {
        return Err(ApiError::BadRequest(
            "only http(s) URLs are allowed for this item type".into(),
        ));
    }
    let id = {
        let conn = state.db.lock();
        let jb = state.jukebox.lock();
        jb.add(
            &conn,
            ty,
            &body.reference,
            body.title.as_deref(),
            "Admin",
            "@admin",
            crate::util::now_unix(),
        )?
    };
    state.broadcast_jukebox();
    Ok(Json(json!({"ok": true, "id": id})))
}

async fn jukebox_remove(
    State(state): State<SharedState>,
    AxPath(id): AxPath<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    let advanced = {
        let conn = state.db.lock();
        let mut jb = state.jukebox.lock();
        let was_current = jb.now_playing == Some(id);
        jb.remove(&conn, id)?;
        if was_current {
            jb.advance(&conn)?; // §8: if current removed, advance
        }
        was_current
    };
    state.broadcast_jukebox();
    Ok(Json(json!({"ok": true, "advanced": advanced})))
}

async fn jukebox_top(
    State(state): State<SharedState>,
    AxPath(id): AxPath<u64>,
) -> ApiResult<Json<serde_json::Value>> {
    {
        let conn = state.db.lock();
        let jb = state.jukebox.lock();
        jb.move_to_top(&conn, id)?;
    }
    state.broadcast_jukebox();
    Ok(Json(json!({"ok": true})))
}

/// `POST /api/jukebox/next` — human-driven advance; the way out of
/// `PlayingExternal` (F10.4, F11.3).
async fn jukebox_next(State(state): State<SharedState>) -> ApiResult<Json<serde_json::Value>> {
    {
        let conn = state.db.lock();
        let mut jb = state.jukebox.lock();
        jb.advance(&conn)?;
    }
    state.broadcast_jukebox();
    Ok(Json(json!({"ok": true})))
}

async fn jukebox_clear(State(state): State<SharedState>) -> ApiResult<Json<serde_json::Value>> {
    {
        let conn = state.db.lock();
        let mut jb = state.jukebox.lock();
        jb.clear(&conn)?;
    }
    state.broadcast_jukebox();
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct ModeBody {
    mode: String,
}

async fn jukebox_mode(
    State(state): State<SharedState>,
    Json(body): Json<ModeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let mode = OrderMode::from_str_lossy(&body.mode);
    {
        let mut jb = state.jukebox.lock();
        jb.set_mode(mode);
        let mut cfg = state.config.write();
        cfg.jukebox_order_mode = mode.as_str().to_string();
        cfg.save(&state.config_path)
            .map_err(|e| ApiError::Internal(format!("save config: {e}")))?;
    }
    state.broadcast_jukebox();
    Ok(Json(json!({"ok": true, "mode": mode.as_str()})))
}

// ─────────────────────────────── log ───────────────────────────────

#[derive(Deserialize)]
struct LogQuery {
    #[serde(default = "default_lines")]
    lines: usize,
}
fn default_lines() -> usize {
    500
}

/// `GET /api/log?lines=N` — tail of the newest server log file (F17.3).
async fn log_tail(
    State(state): State<SharedState>,
    Query(q): Query<LogQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let logs_dir = state.dirs.logs.clone();
    let lines = q.lines.min(5000);
    let out = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<String>> {
        // newest file by name (daily-rotated files sort lexicographically)
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
    .map_err(|e| ApiError::Internal(format!("log task: {e}")))??;
    Ok(Json(json!({"lines": out})))
}
