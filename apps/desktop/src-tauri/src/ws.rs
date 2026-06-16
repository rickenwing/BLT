//! Client `/ws` loop: auto-reconnect with backoff + full state resync on every
//! (re)connect, so the UI never shows stale queue/roster state after a Wi-Fi
//! blip (HARD CONSTRAINT #12 / F15.1-.2). Incoming state lands in
//! `ClientState.live` and is mirrored to the UI via Tauri events.

use crate::state::Shared;
use blt_core::protocol::{ClientMsg, ServerMsg};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tauri::Emitter;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

/// Start (or restart) the WS loop for the currently-configured game endpoint.
/// The task runs until the endpoint changes (it re-reads it each connect).
pub fn spawn(state: Shared, app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut backoff = Duration::from_millis(500);
        loop {
            let Some(game) = state.connection.read().game_endpoint.clone() else {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            };
            match run_once(&state, &app, &game).await {
                Ok(()) => {
                    backoff = Duration::from_millis(500);
                }
                Err(e) => {
                    debug!("ws connect to {game}: {e}");
                }
            }
            // Disconnected: reflect it, back off, reconnect (F15.1).
            state.connection.write().ws_connected = false;
            let _ = app.emit("connection-changed", ());
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    });
}

async fn run_once(state: &Shared, app: &tauri::AppHandle, game: &str) -> anyhow::Result<()> {
    // Bound the connect: a dropped SYN (host briefly unreachable after a server
    // restart, or a firewall re-evaluating the new process) would otherwise
    // block the reconnect loop for the OS TCP timeout (~75s) with no retry —
    // which reads as a permanent "reconnecting…" hang (HARD CONSTRAINT #12).
    let connect = tokio_tungstenite::connect_async(format!("ws://{game}/ws"));
    let (ws, _) = match tokio::time::timeout(Duration::from_secs(6), connect).await {
        Ok(res) => res?,
        Err(_) => anyhow::bail!("connect to {game} timed out"),
    };
    info!("ws connected to {game}");
    let (mut sink, mut stream) = ws.split();

    // Outgoing channel: replace any previous sender.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ClientMsg>();
    *state.ws_out.write() = Some(tx.clone());

    {
        let mut c = state.connection.write();
        c.ws_connected = true;
    }
    let _ = app.emit("connection-changed", ());

    // Hello + resync on every (re)connect (HARD CONSTRAINT #12).
    let (client_id, display_name, mode) = {
        let s = state.settings.read();
        (s.client_id.clone(), s.display_name.clone(), s.mode)
    };
    let machine = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_default();
    let _ = tx.send(ClientMsg::Hello {
        client_id,
        display_name,
        machine_name: machine,
        mode,
    });
    let _ = tx.send(ClientMsg::Resync);
    // Re-announce seeded titles after a reconnect.
    announce_seeds(state, &tx);

    let writer = async {
        // Heartbeat: ping every 20s. A dead link surfaces as a failed send (→
        // reconnect), and keeps the server's read-timeout from dropping us.
        let mut ping = tokio::time::interval(Duration::from_secs(20));
        ping.tick().await; // skip the immediate first tick
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    sink.send(Message::Text(serde_json::to_string(&msg)?)).await?;
                }
                _ = ping.tick() => {
                    sink.send(Message::Text(serde_json::to_string(&ClientMsg::Ping)?)).await?;
                }
            }
        }
        anyhow::Ok(())
    };

    let reader = async {
        loop {
            // Bail if the server goes silent (a half-open drop sends no FIN, so
            // `next()` would otherwise block forever and wedge "connected" with no
            // reconnect — HARD CONSTRAINT #12). Pongs keep a healthy link alive.
            let msg = match tokio::time::timeout(Duration::from_secs(60), stream.next()).await {
                Err(_) => anyhow::bail!("no server activity for 60s"),
                Ok(None) => break,
                Ok(Some(m)) => m?,
            };
            let Message::Text(text) = msg else { continue };
            let Ok(parsed) = serde_json::from_str::<ServerMsg>(&text) else {
                continue;
            };
            handle(state, app, parsed).await;
        }
        anyhow::Ok(())
    };

    tokio::select! {
        r = writer => r?,
        r = reader => r?,
    }
    Ok(())
}

async fn handle(state: &Shared, app: &tauri::AppHandle, msg: ServerMsg) {
    match msg {
        ServerMsg::Welcome {
            server_uuid,
            server_label,
        } => {
            let mut c = state.connection.write();
            c.server_uuid = Some(server_uuid);
            c.server_label = Some(server_label);
            drop(c);
            let _ = app.emit("connection-changed", ());
        }
        ServerMsg::Jukebox(jb) => {
            state.live.write().jukebox = Some(jb);
            let _ = app.emit("jukebox-changed", ());
        }
        ServerMsg::Roster { roster } => {
            state.live.write().roster = roster;
            let _ = app.emit("roster-changed", ());
        }
        ServerMsg::Peers {
            title_id,
            manifest_ver,
            peers,
        } => {
            state
                .live
                .write()
                .peers
                .insert((title_id, manifest_ver), peers);
        }
        ServerMsg::ProbePeers { peers } => {
            // Reachability self-test (F13.4): try a lightweight HTTP request to
            // 1-2 peers; failure → server-only mode, visible not silent (F13.5).
            let state = state.clone();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if peers.is_empty() {
                    return; // nothing to probe yet — stay optimistic
                }
                let mut reachable = false;
                for p in &peers {
                    let url = format!("http://{}/ping", p.chunk_endpoint);
                    let ok = crate::server_api::http()
                        .get(&url)
                        .timeout(Duration::from_secs(3))
                        .send()
                        .await
                        .is_ok();
                    state.send_ws(ClientMsg::ReachabilityResult {
                        peer: p.client_id.clone(),
                        reachable: ok,
                    });
                    if ok {
                        reachable = true;
                        break;
                    }
                }
                state.live.write().p2p_reachable = Some(reachable);
                if !reachable {
                    warn!("P2P probes failed — entering server-only mode (visible in roster)");
                }
                // Surface in our roster entry immediately.
                let activity = if reachable { "idle" } else { "server-only" };
                state.send_ws(ClientMsg::Activity {
                    activity: activity.into(),
                    throughput_bps: None,
                    server_only: !reachable,
                });
                let _ = app.emit("connection-changed", ());
            });
        }
        ServerMsg::TitleChanged { .. } => {
            // Update-available state changes; UI refetches titles (F4.9).
            let _ = app.emit("titles-changed", ());
        }
        ServerMsg::Pong => {}
    }
}

/// (Re)announce every complete download as a seed (after reconnect).
fn announce_seeds(state: &Shared, tx: &tokio::sync::mpsc::UnboundedSender<ClientMsg>) {
    if !state.settings.read().share_back {
        return;
    }
    if state.settings.read().mode == blt_core::protocol::Mode::Playback {
        return; // playback machines never seed (HARD CONSTRAINT #8)
    }
    let Some(port) = *state.seed_port.read() else {
        return;
    };
    let conn = state.db.lock();
    if let Ok(rows) = crate::db::list_downloads(&conn) {
        for r in rows {
            if r.status == "complete" || r.have_chunks > 0 {
                let _ = tx.send(ClientMsg::Announce {
                    title_id: r.title_id,
                    manifest_ver: r.manifest_ver,
                    chunk_endpoint: format!("0.0.0.0:{port}"),
                });
            }
        }
    }
}
