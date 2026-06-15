//! The download engine (M2 + M4): a **visible sequential queue** of title
//! jobs; the active job fetches missing chunks (per the resume bitmap) from
//! the server plus any usable peers, **verifying every chunk before it is
//! written** (HARD CONSTRAINT #1), with retry/backoff across Wi-Fi drops,
//! pause/resume, and quick validation + layout finalisation at completion.
//!
//! Downloads are keyed `(title_id, manifest_ver)` and always complete against
//! the version they started on (F4.9).

use crate::db;
use crate::server_api;
use crate::state::Shared;
use blt_core::bitmap::Bitmap;
use blt_core::manifest::{ChunkLocator, Manifest};
use blt_core::p2p::{assign, PeerRate, PeerSource, SchedulerConfig, SERVER_SOURCE_ID};
use blt_core::transfer::{finalize_layout, validate_quick, verify_and_write};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::Emitter;
use tracing::{info, warn};

/// How many chunk fetches run concurrently within the active job.
const FETCH_CONCURRENCY: usize = 4;
/// Persist the bitmap after this many newly-verified chunks.
const PERSIST_EVERY: u64 = 32;
/// Per-source consecutive-failure limit before the source is dropped (F15.3).
const SOURCE_FAILURE_LIMIT: u32 = 3;

#[derive(Debug, Clone, serde::Serialize)]
pub struct QueueEntry {
    pub title_id: u64,
    pub manifest_ver: u32,
    pub name: String,
    pub dest: String,
    pub status: String, // queued | active | paused | complete | error
    pub total_chunks: u64,
    pub have_chunks: u64,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub speed_bps: u64,
    pub error: Option<String>,
}

struct Job {
    title_id: u64,
    manifest_ver: u32,
    name: String,
    dest: PathBuf,
}

/// Control + progress state shared with the active job task.
#[derive(Default)]
struct Active {
    title_id: u64,
    manifest_ver: u32,
    paused: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    bytes_done: AtomicU64,
    bytes_total: AtomicU64,
    have: AtomicU64,
    total: AtomicU64,
    speed_bps: AtomicU64,
}

pub struct DownloadManager {
    queue: Mutex<VecDeque<Job>>,
    active: RwLock<Option<Arc<Active>>>,
    /// Measured per-peer delivery rates (EWMA), peer id → rate (F13.6).
    rates: Mutex<HashMap<String, PeerRate>>,
    running: AtomicBool,
}

impl Default for DownloadManager {
    fn default() -> Self {
        DownloadManager {
            queue: Mutex::new(VecDeque::new()),
            active: RwLock::new(None),
            rates: Mutex::new(HashMap::new()),
            running: AtomicBool::new(false),
        }
    }
}

impl DownloadManager {
    /// Enqueue a title; the queue is sequential and visible (F-spec M2).
    pub fn enqueue(
        &self,
        state: &Shared,
        app: &tauri::AppHandle,
        title_id: u64,
        manifest_ver: u32,
        name: String,
        dest: PathBuf,
    ) {
        {
            let mut q = self.queue.lock();
            let already = q
                .iter()
                .any(|j| j.title_id == title_id && j.manifest_ver == manifest_ver);
            let active = self
                .active
                .read()
                .as_ref()
                .map(|a| a.title_id == title_id && a.manifest_ver == manifest_ver)
                .unwrap_or(false);
            if already || active {
                return;
            }
            q.push_back(Job {
                title_id,
                manifest_ver,
                name,
                dest,
            });
        }
        self.pump(state, app);
    }

    /// Start the worker for the next job if idle.
    fn pump(&self, state: &Shared, app: &tauri::AppHandle) {
        if self.running.swap(true, Ordering::SeqCst) {
            return; // worker already alive
        }
        let state = state.clone();
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                let job = state.downloads.queue.lock().pop_front();
                let Some(job) = job else { break };
                let res = run_job(&state, &app, &job).await;
                if let Err(e) = res {
                    warn!(title = job.title_id, "download job failed: {e}");
                    let conn = state.db.lock();
                    if let Ok(Some((bm, _, dest))) =
                        db::load_bitmap(&conn, job.title_id, job.manifest_ver)
                    {
                        let _ = db::save_download(
                            &conn,
                            job.title_id,
                            job.manifest_ver,
                            &bm,
                            "error",
                            &dest,
                            Some(&e.to_string()),
                        );
                    }
                }
                *state.downloads.active.write() = None;
                let _ = app.emit("downloads-changed", ());
            }
            state.downloads.running.store(false, Ordering::SeqCst);
        });
    }

    pub fn pause(&self, title_id: u64) {
        if let Some(a) = self.active.read().as_ref() {
            if a.title_id == title_id {
                a.paused.store(true, Ordering::SeqCst);
            }
        }
    }

    pub fn cancel(&self, title_id: u64) {
        self.queue.lock().retain(|j| j.title_id != title_id);
        if let Some(a) = self.active.read().as_ref() {
            if a.title_id == title_id {
                a.cancelled.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Resume a paused/errored download (re-enqueue; the bitmap drives what's left).
    pub fn resume(
        &self,
        state: &Shared,
        app: &tauri::AppHandle,
        title_id: u64,
        manifest_ver: u32,
        name: String,
    ) -> Result<(), String> {
        let dest = {
            let conn = state.db.lock();
            db::load_bitmap(&conn, title_id, manifest_ver)
                .map_err(|e| e.to_string())?
                .map(|(_, _, dest)| PathBuf::from(dest))
                .ok_or("no resume state for this title/version")?
        };
        self.enqueue(state, app, title_id, manifest_ver, name, dest);
        Ok(())
    }

    pub fn record_rate(&self, source: &str, bytes: u64, elapsed: f64) {
        self.rates
            .lock()
            .entry(source.to_string())
            .or_default()
            .record(bytes, elapsed);
    }

    pub fn rate_of(&self, source: &str) -> Option<f64> {
        self.rates
            .lock()
            .get(source)
            .and_then(|r| r.bytes_per_sec())
    }

    /// The queue + active entry as the UI sees it (visible queue, M2).
    pub fn snapshot(&self, state: &Shared) -> Vec<QueueEntry> {
        let mut out = Vec::new();
        if let Some(a) = self.active.read().as_ref() {
            out.push(QueueEntry {
                title_id: a.title_id,
                manifest_ver: a.manifest_ver,
                name: String::new(), // filled from DB rows below if needed
                dest: String::new(),
                status: if a.paused.load(Ordering::SeqCst) {
                    "pausing".into()
                } else {
                    "active".into()
                },
                total_chunks: a.total.load(Ordering::SeqCst),
                have_chunks: a.have.load(Ordering::SeqCst),
                bytes_total: a.bytes_total.load(Ordering::SeqCst),
                bytes_done: a.bytes_done.load(Ordering::SeqCst),
                speed_bps: a.speed_bps.load(Ordering::SeqCst),
                error: None,
            });
        }
        for j in self.queue.lock().iter() {
            out.push(QueueEntry {
                title_id: j.title_id,
                manifest_ver: j.manifest_ver,
                name: j.name.clone(),
                dest: j.dest.to_string_lossy().into_owned(),
                status: "queued".into(),
                total_chunks: 0,
                have_chunks: 0,
                bytes_total: 0,
                bytes_done: 0,
                speed_bps: 0,
                error: None,
            });
        }
        // Persisted rows fill in completed/paused/errored entries.
        let conn = state.db.lock();
        if let Ok(rows) = db::list_downloads(&conn) {
            for r in rows {
                let live = out
                    .iter()
                    .any(|e| e.title_id == r.title_id && e.manifest_ver == r.manifest_ver);
                if !live {
                    out.push(QueueEntry {
                        title_id: r.title_id,
                        manifest_ver: r.manifest_ver,
                        name: String::new(),
                        dest: r.dest_path,
                        status: r.status,
                        total_chunks: r.chunk_count,
                        have_chunks: r.have_chunks,
                        bytes_total: 0,
                        bytes_done: 0,
                        speed_bps: 0,
                        error: r.error,
                    });
                }
            }
        }
        out
    }
}

/// Sources the scheduler may use for the current pass.
fn build_sources(state: &Shared, title_id: u64, manifest_ver: u32) -> Vec<PeerSource> {
    let live = state.live.read();
    let reachable = live.p2p_reachable.unwrap_or(false);
    let peers = live
        .peers
        .get(&(title_id, manifest_ver))
        .cloned()
        .unwrap_or_default();
    drop(live);
    peers
        .into_iter()
        .map(|p| PeerSource {
            id: p.chunk_endpoint.clone(),
            throughput_bps: state.downloads.rate_of(&p.chunk_endpoint),
            reachable,
            server_only: false, // server already filters server-only peers out
            // Peers advertise whole titles in v1; chunk-level have-maps are a
            // refinement — treat an announced peer as having everything and
            // let per-chunk failures blacklist it.
            have: HashSet::new(),
            // DESIGN-NOTE: `have` is filled below per missing set.
        })
        .collect()
}

async fn run_job(state: &Shared, app: &tauri::AppHandle, job: &Job) -> anyhow::Result<()> {
    let game = state
        .connection
        .read()
        .game_endpoint
        .clone()
        .ok_or_else(|| anyhow::anyhow!("not connected"))?;

    info!(title = job.title_id, ver = job.manifest_ver, dest = %job.dest.display(), "download starting");

    // Manifest for the pinned version: refetch and check — if the server
    // republished, we still finish on the version we started (F4.9); the
    // manifest endpoint serves the current one, so only proceed when versions
    // match or this is a fresh start on the current version.
    let manifest: Manifest = server_api::title_manifest(&game, job.title_id)
        .await
        .map_err(|e| anyhow::anyhow!("manifest: {e}"))?;
    if manifest.manifest_ver != job.manifest_ver {
        anyhow::bail!(
            "server now offers v{} (started v{}) — restart the download for the new version",
            manifest.manifest_ver,
            job.manifest_ver
        );
    }

    let locators: Vec<ChunkLocator> = manifest.chunk_locators();
    let total_chunks = manifest.chunk_count();

    // Resume bitmap (the heart of resume — TDD §4.2). If the destination folder
    // is gone (deleted between sessions), the persisted bitmap is stale — start
    // fresh so every chunk is re-fetched instead of "completing" against deleted
    // files and failing validation with no way to recover.
    let mut bitmap = {
        let conn = state.db.lock();
        match db::load_bitmap(&conn, job.title_id, job.manifest_ver)? {
            Some((bm, _, _)) if bm.len() == total_chunks && job.dest.exists() => bm,
            _ => Bitmap::new(total_chunks),
        }
    };

    let active = Arc::new(Active {
        title_id: job.title_id,
        manifest_ver: job.manifest_ver,
        ..Default::default()
    });
    active.total.store(total_chunks, Ordering::SeqCst);
    active.have.store(bitmap.count_set(), Ordering::SeqCst);
    active
        .bytes_total
        .store(manifest.total_size, Ordering::SeqCst);
    active.bytes_done.store(
        locators
            .iter()
            .filter(|l| bitmap.has(l.global_idx))
            .map(|l| l.size)
            .sum(),
        Ordering::SeqCst,
    );
    *state.downloads.active.write() = Some(active.clone());
    persist(state, job, &bitmap, "active", None)?;
    let _ = app.emit("downloads-changed", ());

    // Ask the server for peers (F4.10); answer arrives over /ws.
    state.send_ws(blt_core::protocol::ClientMsg::RequestPeers {
        title_id: job.title_id,
        manifest_ver: job.manifest_ver,
    });
    report_activity(state, &format!("downloading {}", job.name));

    let mut since_persist = 0u64;
    let mut window_start = Instant::now();
    let mut window_bytes = 0u64;
    let mut validate_tries = 0u32;
    let mut failures: HashMap<String, u32> = HashMap::new();
    let mut backoff = Duration::from_millis(500);

    'outer: loop {
        if active.cancelled.load(Ordering::SeqCst) {
            persist(state, job, &bitmap, "paused", None)?;
            info!(
                title = job.title_id,
                "download cancelled (state kept for resume)"
            );
            return Ok(());
        }
        if active.paused.load(Ordering::SeqCst) {
            persist(state, job, &bitmap, "paused", None)?;
            info!(title = job.title_id, "download paused");
            report_activity(state, "idle");
            return Ok(());
        }

        let missing: Vec<u64> = bitmap.missing().collect();
        if missing.is_empty() {
            // Everything present per the bitmap → finalize + quick-validate. If
            // files are missing/corrupt on disk (e.g. the folder was deleted),
            // clear those files' chunks and re-fetch them, rather than leaving a
            // full bitmap that re-fails validation forever (Resume self-heals).
            finalize_layout(&manifest, &job.dest)?;
            let report = validate_quick(&manifest, &job.dest);
            if report.all_ok() {
                break 'outer;
            }
            let failed: HashSet<String> = report.failures().map(|f| f.rel_path.clone()).collect();
            for loc in &locators {
                if failed.contains(&loc.rel_path) {
                    bitmap.clear(loc.global_idx);
                }
            }
            active.have.store(bitmap.count_set(), Ordering::SeqCst);
            validate_tries += 1;
            if validate_tries >= 3 {
                let names: Vec<&String> = failed.iter().take(5).collect();
                persist(
                    state,
                    job,
                    &bitmap,
                    "error",
                    Some(&format!(
                        "validation still failing after re-fetch: {names:?}"
                    )),
                )?;
                anyhow::bail!("quick validation failed after re-fetch: {names:?}");
            }
            warn!(
                title = job.title_id,
                files = failed.len(),
                "validation failed — re-fetching"
            );
            persist(state, job, &bitmap, "active", None)?;
            continue 'outer;
        }

        // Throughput-weighted source assignment (F13.7). Peers that have
        // failed too often this job are dropped (F15.3).
        let mut sources = build_sources(state, job.title_id, job.manifest_ver);
        sources.retain(|s| failures.get(&s.id).copied().unwrap_or(0) < SOURCE_FAILURE_LIMIT);
        for s in &mut sources {
            s.have = missing.iter().copied().collect();
        }
        let batch: Vec<u64> = missing.iter().copied().take(64).collect();
        let assignments = assign(&batch, &sources, &SchedulerConfig::default());

        // Fetch + verify + write the batch group-by-group, dropping each chunk's
        // bytes right after it lands. Peak memory stays bounded to one in-flight
        // group (FETCH_CONCURRENCY chunks) instead of the whole 64-chunk batch —
        // important because chunk_size is server-configurable, so accumulating
        // the batch could pin 64 × chunk_size in RAM.
        let mut any_ok = false;
        for group in assignments.chunks(FETCH_CONCURRENCY) {
            let futs = group.iter().map(|(gidx, source)| {
                let loc = locators[*gidx as usize].clone();
                let game = game.clone();
                let source = source.clone();
                async move {
                    let endpoint = if source == SERVER_SOURCE_ID {
                        game.clone()
                    } else {
                        source.clone()
                    };
                    let started = Instant::now();
                    let res = server_api::fetch_chunk(&endpoint, loc.file_id, loc.chunk_idx)
                        .await
                        .map_err(|e| e.to_string());
                    (loc.global_idx, source, res, started.elapsed().as_secs_f64())
                }
            });
            for (gidx, source, res, elapsed) in futures_util::future::join_all(futs).await {
                if apply_result(
                    state,
                    job,
                    &locators,
                    &mut bitmap,
                    &active,
                    &mut failures,
                    &mut window_bytes,
                    gidx,
                    source,
                    res,
                    elapsed,
                ) {
                    any_ok = true;
                    since_persist += 1;
                }
            }

            if active.paused.load(Ordering::SeqCst) || active.cancelled.load(Ordering::SeqCst) {
                persist(state, job, &bitmap, "active", None)?;
                continue 'outer;
            }
        }

        // Speed window + periodic persistence.
        let elapsed = window_start.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            active
                .speed_bps
                .store((window_bytes as f64 / elapsed) as u64, Ordering::SeqCst);
            window_start = Instant::now();
            window_bytes = 0;
            let _ = app.emit("downloads-changed", ());
            // Roster activity w/ measured seed speed (F13.1 uses seed rate; we
            // report download activity here, seed speed comes from the seeder).
        }
        if since_persist >= PERSIST_EVERY {
            persist(state, job, &bitmap, "active", None)?;
            since_persist = 0;
        }

        if !any_ok {
            // Transient network trouble (Wi-Fi drop): retry with backoff and
            // keep retrying — the download survives and continues (F4.7).
            warn!(
                title = job.title_id,
                "no chunks succeeded; backing off {:?}", backoff
            );
            persist(state, job, &bitmap, "active", None)?;
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        } else {
            backoff = Duration::from_millis(500);
        }
    }

    // The loop only breaks once finalize + quick-validation passed (F5.1).
    persist(state, job, &bitmap, "complete", None)?;
    info!(title = job.title_id, "download complete + validated");
    report_activity(state, "idle");

    // Become a seed for this title if share-back is on (F4.11/F4.12).
    if state.settings.read().share_back {
        if let Some(port) = *state.seed_port.read() {
            state.send_ws(blt_core::protocol::ClientMsg::Announce {
                title_id: job.title_id,
                manifest_ver: job.manifest_ver,
                chunk_endpoint: format!("0.0.0.0:{port}"),
            });
        }
    }
    let _ = app.emit("download-complete", job.title_id);
    Ok(())
}

/// Verify-and-write one fetched chunk; updates bitmap/progress/failure book-
/// keeping. Returns true when the chunk was accepted.
#[allow(clippy::too_many_arguments)]
fn apply_result(
    state: &Shared,
    job: &Job,
    locators: &[ChunkLocator],
    bitmap: &mut Bitmap,
    active: &Arc<Active>,
    failures: &mut HashMap<String, u32>,
    window_bytes: &mut u64,
    gidx: u64,
    source: String,
    res: Result<Vec<u8>, String>,
    elapsed: f64,
) -> bool {
    let loc = &locators[gidx as usize];
    match res {
        Ok(bytes) => {
            // HARD CONSTRAINT #1: BLAKE3-verify before write; a bad chunk is
            // refetched elsewhere, never written.
            match verify_and_write(&job.dest, &loc.rel_path, loc.offset, &bytes, &loc.hash) {
                Ok(()) => {
                    bitmap.set(gidx);
                    active.have.fetch_add(1, Ordering::SeqCst);
                    active.bytes_done.fetch_add(loc.size, Ordering::SeqCst);
                    *window_bytes += loc.size;
                    if source != SERVER_SOURCE_ID {
                        state.downloads.record_rate(&source, loc.size, elapsed);
                    }
                    failures.remove(&source);
                    true
                }
                Err(e) => {
                    warn!(chunk = gidx, %source, "chunk rejected: {e}");
                    *failures.entry(source).or_insert(0) += 1;
                    false
                }
            }
        }
        Err(e) => {
            warn!(chunk = gidx, %source, "chunk fetch failed: {e}");
            *failures.entry(source).or_insert(0) += 1;
            false
        }
    }
}

fn persist(
    state: &Shared,
    job: &Job,
    bitmap: &Bitmap,
    status: &str,
    error: Option<&str>,
) -> anyhow::Result<()> {
    let conn = state.db.lock();
    db::save_download(
        &conn,
        job.title_id,
        job.manifest_ver,
        bitmap,
        status,
        &job.dest.to_string_lossy(),
        error,
    )?;
    Ok(())
}

fn report_activity(state: &Shared, activity: &str) {
    let server_only = state.live.read().p2p_reachable == Some(false);
    state.send_ws(blt_core::protocol::ClientMsg::Activity {
        activity: activity.to_string(),
        throughput_bps: state.downloads.rate_of("@self-seed").map(|r| r as u64),
        server_only,
    });
}
