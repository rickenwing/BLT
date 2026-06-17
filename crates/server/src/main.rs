//! blt-server entry point: data root, config, logging, DB (WAL), listeners,
//! mDNS, periodic tasks.
//!
//! CLI flags (no clap dependency for three flags):
//! - `--data-root <path>`  override the per-OS data root (TDD §17.2)
//! - `--reset-admin-bind`  reset the admin listener to 127.0.0.1:7402 (TDD §11
//!   recovery unlock)
//! - `--headless`          accepted for symmetry; the server currently always
//!   runs headless (tray UX lands in M8 polish)

use blt_core::runtime::{Component, DataRoot, logging};
use blt_server::config::Config;
use blt_server::state::AppState;
use blt_server::{Supervisor, db, jukebox, mdns, tasks};
use std::path::PathBuf;
use tracing::{error, info};

fn main() -> anyhow::Result<()> {
    // ── CLI ──
    let mut data_root_override: Option<PathBuf> = None;
    let mut reset_admin_bind = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-root" => {
                data_root_override = args.next().map(PathBuf::from);
                if data_root_override.is_none() {
                    eprintln!("--data-root requires a path");
                    std::process::exit(2);
                }
            }
            "--reset-admin-bind" => reset_admin_bind = true,
            "--headless" => {}
            "--version" | "-V" => {
                println!("blt-server {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!(
                    "usage: blt-server [--data-root <path>] [--reset-admin-bind] [--headless]"
                );
                std::process::exit(2);
            }
        }
    }

    // ── data root + logging (HARD CONSTRAINT #14/#16) ──
    let root = DataRoot::resolve(data_root_override)?;
    let dirs = root.component_dirs(Component::Server);
    dirs.ensure()?;
    let _log_guard = logging::init(
        &dirs.logs,
        "blt-server",
        "info,blt_server=info,blt_core=info",
    );
    info!(
        version = env!("CARGO_PKG_VERSION"),
        data_root = %root.path().display(),
        "Buttz LAN Tool server starting"
    );

    // ── config (config.toml authoritative; recovery-friendly, F1.4) ──
    let mut config = Config::load_or_create(&dirs.config_path)?;
    if reset_admin_bind {
        config.admin_panel_bind = format!("127.0.0.1:{}", blt_server::config::DEFAULT_ADMIN_PORT);
        config.save(&dirs.config_path)?;
        info!("admin bind reset to {}", config.admin_panel_bind);
    }

    // ── SQLite (WAL) ──
    let conn = db::open(&dirs.db_path)?;
    // Mirror a few config keys into the DB (F1.3 dual persistence).
    db::set_config(&conn, "server_uuid", &config.server_uuid)?;
    db::set_config(&conn, "chunk_size", &config.chunk_size.to_string())?;

    let state = AppState::new(conn, config, dirs.config_path.clone(), dirs.clone());

    // Restore jukebox queue state (persisted across restarts, F8.8).
    {
        let conn = state.db.lock();
        let mode = state.config.read().order_mode();
        match jukebox::restore(&conn, mode) {
            Ok(jb) => *state.jukebox.lock() = jb,
            Err(e) => error!("jukebox restore: {e}"),
        }
    }

    // Load published manifests into the serving cache.
    if let Err(e) = state.reload_manifest_cache() {
        error!("manifest cache load: {e}");
    }

    // ── async runtime ──
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let supervisor = Supervisor::new(state.clone());
        let (game, share, admin) = supervisor.start().await;
        info!(?game, ?share, ?admin, "listeners started");
        if admin.is_none() {
            error!(
                "admin panel failed to bind — fix admin_panel_bind in {} and restart, \
                 or run with --reset-admin-bind",
                state.config_path.display()
            );
        }

        // mDNS advertisement (manual IP entry always works as fallback, F3.2).
        let _mdns_daemon = mdns::advertise(&state);

        // Initial scan if a library is configured.
        if state.library_path().is_some() {
            let st = state.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = blt_server::library::scan_library(&st) {
                    error!("initial scan: {e}");
                }
            });
        }

        tasks::spawn_all(state.clone());

        // Run until ctrl-c.
        let _ = tokio::signal::ctrl_c().await;
        info!("shutdown requested");
    });

    Ok(())
}
