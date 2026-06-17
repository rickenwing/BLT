//! Periodic background tasks: auto-scan (F2.5), staging settle/promote (F2.2),
//! and the SQLite backup snapshot (F15.7 / HARD CONSTRAINT #13).

use crate::db;
use crate::library::{self, StagingTracker};
use crate::state::SharedState;
use std::collections::HashSet;
use std::ffi::OsString;
use std::time::{Duration, SystemTime};
use tracing::{error, info, warn};

/// Spawn all periodic tasks. Each reads its interval from config every cycle so
/// admin changes apply without restart.
pub fn spawn_all(state: SharedState) {
    tokio::spawn(periodic_scan(state.clone()));
    tokio::spawn(staging_watch(state.clone()));
    tokio::spawn(share_orphan_sweep(state.clone()));
    tokio::spawn(db_backup(state));
}

/// How often to sweep, and the minimum age before an unknown entry is removed.
const SWEEP_INTERVAL_SECS: u64 = 300;
const ORPHAN_MIN_AGE_SECS: u64 = 1800;

/// Remove share files/dirs under the share root that have **no DB row** — orphans
/// left by a delete-while-streaming race (esp. on Windows, where the immediate
/// delete can't remove an in-use file) or an abandoned upload. Age-guarded: an
/// in-progress upload writes its files *before* its DB row exists, so only
/// entries untouched for `ORPHAN_MIN_AGE_SECS` are eligible.
async fn share_orphan_sweep(state: SharedState) {
    loop {
        tokio::time::sleep(Duration::from_secs(SWEEP_INTERVAL_SECS)).await;
        let st = state.clone();
        let _ = tokio::task::spawn_blocking(move || sweep_share_orphans(&st)).await;
    }
}

fn sweep_share_orphans(state: &SharedState) {
    let Some(root) = state.share_path() else {
        return;
    };
    let valid: HashSet<OsString> = {
        let conn = state.db.lock();
        db::all_stored_paths(&conn)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| {
                std::path::Path::new(&p)
                    .file_name()
                    .map(|n| n.to_os_string())
            })
            .collect()
    };
    let Ok(rd) = std::fs::read_dir(&root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in rd.flatten() {
        if valid.contains(&entry.file_name()) {
            continue;
        }
        // Skip recent entries — an in-progress (or just-finished) upload has
        // files on disk before its DB row is committed.
        let recent = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| now.duration_since(t).map(|d| d.as_secs()).unwrap_or(0) < ORPHAN_MIN_AGE_SECS)
            .unwrap_or(true);
        if recent {
            continue;
        }
        let p = entry.path();
        let r = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        match r {
            Ok(()) => info!(path = %p.display(), "swept orphaned share data"),
            Err(e) => warn!(path = %p.display(), "orphan sweep skipped (in use?): {e}"),
        }
    }
}

async fn periodic_scan(state: SharedState) {
    loop {
        let interval = state.config.read().scan_interval_secs.max(30);
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if state.library_path().is_none() {
            continue;
        }
        let st = state.clone();
        let res = tokio::task::spawn_blocking(move || library::scan_library(&st)).await;
        match res {
            Ok(Ok(s)) => {
                if !s.published.is_empty() || !s.republished.is_empty() || !s.removed.is_empty() {
                    info!(?s.published, ?s.republished, ?s.removed, "periodic scan changes");
                }
            }
            Ok(Err(e)) => warn!("periodic scan: {e}"),
            Err(e) => error!("periodic scan task: {e}"),
        }
    }
}

async fn staging_watch(state: SharedState) {
    let mut tracker = StagingTracker::default();
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let promoted = {
            let st = state.clone();
            // tick() does file IO; it's cheap stat-walking but still blocking.
            let mut t = std::mem::take(&mut tracker);
            let res = tokio::task::spawn_blocking(move || {
                let p = t.tick(&st);
                (t, p)
            })
            .await;
            match res {
                Ok((t, p)) => {
                    tracker = t;
                    p
                }
                Err(e) => {
                    error!("staging tick: {e}");
                    continue;
                }
            }
        };
        if !promoted.is_empty() {
            // Newly promoted titles get scanned immediately.
            let st = state.clone();
            match tokio::task::spawn_blocking(move || library::scan_library(&st)).await {
                Ok(Ok(_)) => info!(?promoted, "promoted titles scanned"),
                Ok(Err(e)) => warn!("post-promote scan: {e}"),
                Err(e) => error!("post-promote scan task: {e}"),
            }
        }
    }
}

/// Periodic SQLite snapshot into `backups/`, keeping the newest few.
async fn db_backup(state: SharedState) {
    loop {
        let interval = state.config.read().db_backup_interval_secs.max(60);
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let st = state.clone();
        let res = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let stamp = crate::util::now_unix();
            let dest = st.dirs.backups.join(format!("blt-server-{stamp}.db"));
            {
                // One -1 step copies all pages at once: the connection mutex
                // is held for the minimum time instead of sleeping between
                // batches while every DB-touching endpoint stalls.
                let conn = st.db.lock();
                let mut dst = rusqlite::Connection::open(&dest)?;
                let backup = rusqlite::backup::Backup::new(&conn, &mut dst)?;
                backup.step(-1)?;
            }
            // Retention: keep the 5 newest snapshots.
            let mut snaps: Vec<_> = std::fs::read_dir(&st.dirs.backups)?
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.extension().is_some_and(|e| e == "db")
                        && p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("blt-server-"))
                })
                .collect();
            snaps.sort();
            while snaps.len() > 5 {
                let old = snaps.remove(0);
                let _ = std::fs::remove_file(old);
            }
            Ok(())
        })
        .await;
        match res {
            Ok(Ok(())) => info!("db backup snapshot written"),
            Ok(Err(e)) => warn!("db backup failed: {e}"),
            Err(e) => error!("db backup task: {e}"),
        }
    }
}
