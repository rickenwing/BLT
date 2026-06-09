//! mDNS browse (F3.1/F3.3): discover `_blt._tcp` servers, parse the IP:port
//! TXT contract from core, persist into the `servers` table, notify the UI.
//! Manual IP entry is always available as the fallback (F3.2).

use crate::db;
use crate::state::Shared;
use blt_core::discovery::{parse_txt, SERVICE_TYPE};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use tauri::Emitter;
use tracing::{info, warn};

pub fn spawn(state: Shared, app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let daemon = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                warn!("mdns browse unavailable (manual connect still works): {e}");
                return;
            }
        };
        let receiver = match daemon.browse(SERVICE_TYPE) {
            Ok(r) => r,
            Err(e) => {
                warn!("mdns browse failed: {e}");
                return;
            }
        };
        info!("mDNS browsing for {SERVICE_TYPE}");
        while let Ok(event) = receiver.recv() {
            if let ServiceEvent::ServiceResolved(srv) = event {
                let props: std::collections::HashMap<String, String> = srv
                    .get_properties()
                    .iter()
                    .map(|p| (p.key().to_string(), p.val_str().to_string()))
                    .collect();
                if let Some(found) = parse_txt(&props) {
                    info!(label = %found.label, game = %found.game_endpoint, "discovered server");
                    {
                        let conn = state.db.lock();
                        let _ = db::upsert_server(
                            &conn,
                            Some(&found.uuid),
                            &found.label,
                            &found.game_endpoint,
                            &found.share_endpoint,
                            now(),
                        );
                    }
                    let _ = app.emit("servers-changed", ());
                }
            }
        }
    });
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
