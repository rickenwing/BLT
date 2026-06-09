//! Periodic background tasks: auto-scan (F2.5), staging settle/promote (F2.2),
//! and the SQLite backup snapshot (F15.7 / HARD CONSTRAINT #13).

use crate::library::{self, StagingTracker};
use crate::state::SharedState;
use std::time::Duration;
use tracing::{error, info, warn};

/// Spawn all periodic tasks. Each reads its interval from config every cycle so
/// admin changes apply without restart.
pub fn spawn_all(state: SharedState) {
    tokio::spawn(periodic_scan(state.clone()));
    tokio::spawn(staging_watch(state.clone()));
    tokio::spawn(db_backup(state));
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
