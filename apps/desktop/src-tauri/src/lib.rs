//! blt — the Buttz LAN Tool desktop app (client + playback modes).

pub mod chunk_server;
pub mod commands;
pub mod db;
pub mod discovery;
pub mod downloads;
pub mod freespace;
pub mod scripts;
pub mod server_api;
pub mod state;
pub mod ws;

use blt_core::runtime::{logging, Component, DataRoot};
use state::{ClientState, Settings};
use std::sync::Arc;
use tracing::{error, info};

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Data root + logging (TDD §17; HARD CONSTRAINT #14/#16).
    let override_root = std::env::var_os("BLT_DATA_ROOT").map(std::path::PathBuf::from);
    let root = match DataRoot::resolve(override_root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot resolve data root: {e}");
            std::process::exit(1);
        }
    };
    let dirs = root.component_dirs(Component::Client);
    if let Err(e) = dirs.ensure() {
        eprintln!("cannot create data folders: {e}");
        std::process::exit(1);
    }
    let log_guard = logging::init(&dirs.logs, "blt-client", "info,blt_app=info,blt_core=info");
    info!(
        version = env!("CARGO_PKG_VERSION"),
        data_root = %root.path().display(),
        "Buttz LAN Tool client starting"
    );

    let conn = match db::open(&dirs.db_path) {
        Ok(c) => c,
        Err(e) => {
            error!("client db: {e}");
            std::process::exit(1);
        }
    };
    let settings = Settings::load(&conn);
    let last_server = settings.last_server.clone();

    let client_state: state::Shared = Arc::new(ClientState {
        db: parking_lot::Mutex::new(conn),
        dirs,
        settings: parking_lot::RwLock::new(settings),
        connection: parking_lot::RwLock::new(state::Connection_::default()),
        live: parking_lot::RwLock::new(state::LiveState::default()),
        downloads: downloads::DownloadManager::default(),
        ws_out: parking_lot::RwLock::new(None),
        seed_port: parking_lot::RwLock::new(None),
        app: std::sync::OnceLock::new(),
        xfers: parking_lot::Mutex::new(std::collections::HashMap::new()),
        xfer_seq: std::sync::atomic::AtomicU64::new(1),
    });

    // Reconnect to the last server automatically (F3.4).
    if let Some(game) = last_server {
        let mut c = client_state.connection.write();
        c.game_endpoint = Some(game.clone());
        // The share endpoint is refreshed from discovery rows when available.
        drop(c);
        let conn = client_state.db.lock();
        if let Ok(rows) = db::list_servers(&conn) {
            if let Some(row) = rows
                .iter()
                .find(|r| r.game_endpoint.as_deref() == Some(&game))
            {
                client_state.connection.write().share_endpoint = row.share_endpoint.clone();
                client_state.connection.write().server_label = row.label.clone();
            }
        }
    }

    let boot_state = client_state.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(client_state)
        .invoke_handler(tauri::generate_handler![
            commands::get_app_state,
            commands::choose_mode,
            commands::update_settings,
            commands::list_servers,
            commands::connect_to,
            commands::connection_state,
            commands::fetch_titles,
            commands::fetch_title_info,
            commands::prepare_download,
            commands::begin_download,
            commands::pause_download,
            commands::resume_download,
            commands::cancel_download,
            commands::delete_game,
            commands::downloads_snapshot,
            commands::validate_title,
            commands::repair_title,
            commands::launch_title,
            commands::script_preview,
            commands::script_run,
            commands::shares_list,
            commands::share_listing,
            commands::share_upload,
            commands::share_download,
            commands::share_delete,
            commands::preflight_dest,
            commands::jukebox_state,
            commands::jukebox_add,
            commands::jukebox_vote,
            commands::jukebox_next,
            commands::jukebox_ended,
            commands::roster,
            commands::resolve_share_stream,
            commands::lockdown_enter,
            commands::lockdown_exit,
            commands::log_tail,
            commands::external_open,
            commands::update_check,
            commands::update_install,
            commands::active_transfers,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            // Keep the logging guard alive for the app lifetime.
            Box::leak(Box::new(log_guard));
            // Let state emit events (transfer progress, etc.).
            let _ = boot_state.app.set(handle.clone());

            // Background services: WS live channel, mDNS browse, seed server.
            ws::spawn(boot_state.clone(), handle.clone());
            discovery::spawn(boot_state.clone(), handle.clone());
            let seed_state = boot_state.clone();
            tauri::async_runtime::spawn(async move {
                chunk_server::start(seed_state).await;
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
