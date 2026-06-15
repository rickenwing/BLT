//! Shared application state (`AppState`), held in an `Arc` and passed to every
//! handler. The SQLite connection is behind a `Mutex` and **never locked across
//! an `.await`**; published manifests are cached in an `RwLock<HashMap>` for the
//! lock-light hot path (chunk serving).

use crate::config::Config;
use crate::jukebox::Jukebox;
use crate::ws::SessionRegistry;
use blt_core::manifest::Manifest;
use blt_core::runtime::ComponentDirs;
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

// Admin session + brute-force policy (SEC-2 / SEC-5).
const SESSION_IDLE: Duration = Duration::from_secs(2 * 3600); // 2h idle
const SESSION_ABSOLUTE: Duration = Duration::from_secs(12 * 3600); // 12h max
const MAX_LOGIN_FAILS: u32 = 8;
const LOGIN_LOCKOUT: Duration = Duration::from_secs(60);

/// Which listener to rebind after a config change (F1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceKind {
    Game,
    Share,
    Admin,
}

/// The big-data paths configured separately from the data root (TDD §17.2).
#[derive(Debug, Clone, Default)]
pub struct ServerPaths {
    pub library: Option<PathBuf>,
    pub staging: Option<PathBuf>,
    pub share: Option<PathBuf>,
}

pub struct AppState {
    pub db: Mutex<Connection>,
    pub config: RwLock<Config>,
    pub config_path: PathBuf,
    pub dirs: ComponentDirs,
    pub paths: RwLock<ServerPaths>,
    /// Published manifests by title id — the hot-path serving cache.
    pub manifests: RwLock<HashMap<u64, Arc<Manifest>>>,
    pub jukebox: Mutex<Jukebox>,
    pub sessions: Mutex<SessionRegistry>,
    /// Live admin sessions: cookie token → timestamps (idle + absolute expiry,
    /// SEC-5). In-memory: an admin logs in again after a server restart.
    pub admin_sessions: Mutex<HashMap<String, SessionMeta>>,
    /// Per-IP brute-force throttle for the password gates (SEC-2).
    pub login_throttle: Mutex<HashMap<std::net::IpAddr, LoginThrottle>>,
    /// Set by the supervisor; lets the admin API request a listener rebind.
    pub rebind: OnceLock<tokio::sync::mpsc::UnboundedSender<ServiceKind>>,
    pub started: Instant,
}

/// One admin session's lifecycle timestamps (SEC-5).
#[derive(Clone, Copy)]
pub struct SessionMeta {
    pub created: Instant,
    pub last_seen: Instant,
}

/// Per-IP failed-attempt tracking for the password gates (SEC-2).
#[derive(Default, Clone, Copy)]
pub struct LoginThrottle {
    pub fails: u32,
    pub locked_until: Option<Instant>,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    pub fn new(
        db: Connection,
        config: Config,
        config_path: PathBuf,
        dirs: ComponentDirs,
    ) -> Arc<Self> {
        let paths = ServerPaths {
            library: config.library_path.clone(),
            staging: config.staging_path.clone(),
            share: config.share_path.clone(),
        };
        let jukebox = Jukebox::new(config.order_mode());
        Arc::new(AppState {
            db: Mutex::new(db),
            config: RwLock::new(config),
            config_path,
            dirs,
            paths: RwLock::new(paths),
            manifests: RwLock::new(HashMap::new()),
            jukebox: Mutex::new(jukebox),
            sessions: Mutex::new(SessionRegistry::default()),
            admin_sessions: Mutex::new(HashMap::new()),
            login_throttle: Mutex::new(HashMap::new()),
            rebind: OnceLock::new(),
            started: Instant::now(),
        })
    }

    // ── admin sessions (SEC-5) ──

    /// Mint a fresh session token.
    pub fn session_new(&self) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        let now = Instant::now();
        self.admin_sessions.lock().insert(
            token.clone(),
            SessionMeta {
                created: now,
                last_seen: now,
            },
        );
        token
    }

    /// Validate a token, enforcing idle + absolute expiry and refreshing the
    /// idle clock; expired tokens are pruned.
    pub fn session_valid(&self, token: &str) -> bool {
        let now = Instant::now();
        let mut m = self.admin_sessions.lock();
        match m.get(token).copied() {
            Some(s)
                if now.duration_since(s.created) < SESSION_ABSOLUTE
                    && now.duration_since(s.last_seen) < SESSION_IDLE =>
            {
                if let Some(e) = m.get_mut(token) {
                    e.last_seen = now;
                }
                true
            }
            Some(_) => {
                m.remove(token);
                false
            }
            None => false,
        }
    }

    pub fn session_remove(&self, token: &str) {
        self.admin_sessions.lock().remove(token);
    }

    /// Revoke every session (used on password change, SEC-5).
    pub fn session_clear(&self) {
        self.admin_sessions.lock().clear();
    }

    // ── login brute-force throttle (SEC-2) ──

    /// True while `ip` is locked out for repeated failures.
    pub fn login_locked(&self, ip: IpAddr) -> bool {
        match self
            .login_throttle
            .lock()
            .get(&ip)
            .and_then(|e| e.locked_until)
        {
            Some(until) => Instant::now() < until,
            None => false,
        }
    }

    pub fn login_fail(&self, ip: IpAddr) {
        let mut t = self.login_throttle.lock();
        let e = t.entry(ip).or_default();
        e.fails += 1;
        if e.fails >= MAX_LOGIN_FAILS {
            e.locked_until = Some(Instant::now() + LOGIN_LOCKOUT);
            e.fails = 0;
        }
    }

    pub fn login_ok(&self, ip: IpAddr) {
        self.login_throttle.lock().remove(&ip);
    }

    /// (Re)load all published manifests from the DB into the serving cache.
    /// A title with corrupt rows is skipped (and loudly logged) rather than
    /// taking every other title offline with it.
    pub fn reload_manifest_cache(&self) -> anyhow::Result<()> {
        let conn = self.db.lock();
        let titles = crate::db::list_titles(&conn)?;
        let mut cache = HashMap::new();
        for t in titles {
            if t.state == "published" {
                match crate::db::load_manifest(&conn, t.id) {
                    Ok(Some(m)) => {
                        cache.insert(t.id, Arc::new(m));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!(
                            title = %t.name,
                            "manifest unloadable (corrupt DB rows?) — title NOT served: {e}"
                        );
                    }
                }
            }
        }
        drop(conn);
        *self.manifests.write() = cache;
        Ok(())
    }

    /// Update one title's cached manifest (after a (re)scan).
    pub fn cache_manifest(&self, manifest: Manifest) {
        self.manifests
            .write()
            .insert(manifest.title_id, Arc::new(manifest));
    }

    pub fn cached_manifest(&self, title_id: u64) -> Option<Arc<Manifest>> {
        self.manifests.read().get(&title_id).cloned()
    }

    pub fn library_path(&self) -> Option<PathBuf> {
        self.paths.read().library.clone()
    }

    pub fn share_path(&self) -> Option<PathBuf> {
        self.paths.read().share.clone()
    }

    pub fn staging_path(&self) -> Option<PathBuf> {
        self.paths.read().staging.clone()
    }

    /// Broadcast the jukebox state to all `/ws` consumers, personalised per
    /// client so each sees its own `voted_by_me` (F8.4 toggle UI).
    pub fn broadcast_jukebox(&self) {
        let conns = self.sessions.lock().conns_snapshot();
        let conn = self.db.lock();
        let jb = self.jukebox.lock();
        let sessions = self.sessions.lock();
        for (conn_id, client_id) in conns {
            if let Ok(state) = jb.state_for(&conn, &client_id) {
                sessions.send_to_conn(conn_id, blt_core::protocol::ServerMsg::Jukebox(state));
            }
        }
    }

    /// Broadcast the current roster to all `/ws` consumers.
    pub fn broadcast_roster(&self) {
        let sessions = self.sessions.lock();
        let roster = sessions.roster();
        sessions.broadcast(blt_core::protocol::ServerMsg::Roster { roster });
    }
}
