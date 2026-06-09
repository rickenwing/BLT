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
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

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
    /// Live admin session tokens (cookie values). In-memory: an admin logs in
    /// again after a server restart.
    pub admin_sessions: Mutex<HashSet<String>>,
    /// Set by the supervisor; lets the admin API request a listener rebind.
    pub rebind: OnceLock<tokio::sync::mpsc::UnboundedSender<ServiceKind>>,
    pub started: Instant,
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
            admin_sessions: Mutex::new(HashSet::new()),
            rebind: OnceLock::new(),
            started: Instant::now(),
        })
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
