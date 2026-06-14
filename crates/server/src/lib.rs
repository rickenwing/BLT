//! blt-server library: module tree + the listener supervisor. `main.rs` is a
//! thin wrapper so integration tests can boot the whole server in-process.

pub mod admin_api;
pub mod bindings;
pub mod config;
pub mod data_api;
pub mod db;
pub mod error;
pub mod jukebox;
pub mod library;
pub mod mdns;
pub mod share;
pub mod state;
pub mod tasks;
pub mod util;
pub mod ws;

use crate::state::{ServiceKind, SharedState};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{error, info, warn};

/// The three independently-bindable listeners (TDD §3.1). Each runs until its
/// shutdown signal fires; the supervisor restarts one on a rebind request
/// without touching the others (F1.3).
pub struct Supervisor {
    state: SharedState,
}

struct Listener {
    kind: ServiceKind,
    shutdown: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl Supervisor {
    pub fn new(state: SharedState) -> Self {
        Supervisor { state }
    }

    /// Boot all three listeners and the rebind loop. Resolves once all initial
    /// binds have been attempted; returns the bound addresses
    /// `(game, share, admin)` (None = that listener failed to bind).
    pub async fn start(&self) -> (Option<SocketAddr>, Option<SocketAddr>, Option<SocketAddr>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ServiceKind>();
        let _ = self.state.rebind.set(tx);

        let game = spawn_listener(self.state.clone(), ServiceKind::Game).await;
        let share = spawn_listener(self.state.clone(), ServiceKind::Share).await;
        let admin = spawn_listener(self.state.clone(), ServiceKind::Admin).await;

        let bound = (
            game.as_ref().map(|l| l.1),
            share.as_ref().map(|l| l.1),
            admin.as_ref().map(|l| l.1),
        );

        // The rebind loop owns the listener handles.
        let state = self.state.clone();
        let mut listeners: Vec<Listener> = [game, share, admin]
            .into_iter()
            .flatten()
            .map(|(l, _)| l)
            .collect();
        tokio::spawn(async move {
            while let Some(kind) = rx.recv().await {
                info!(?kind, "rebinding listener");
                if let Some(pos) = listeners.iter().position(|l| l.kind == kind) {
                    let old = listeners.remove(pos);
                    let _ = old.shutdown.send(true);
                    let _ = old.handle.await;
                }
                match spawn_listener(state.clone(), kind).await {
                    Some((l, addr)) => {
                        info!(?kind, %addr, "listener rebound");
                        listeners.push(l);
                    }
                    None => error!(
                        ?kind,
                        "rebind failed; service is DOWN until config is fixed"
                    ),
                }
            }
        });

        bound
    }
}

/// Bind + serve one service. Returns the listener handle and the bound address.
async fn spawn_listener(state: SharedState, kind: ServiceKind) -> Option<(Listener, SocketAddr)> {
    let bind = {
        let cfg = state.config.read();
        match kind {
            ServiceKind::Game => cfg.game_bind(),
            ServiceKind::Share => cfg.share_bind(),
            ServiceKind::Admin => cfg.admin_bind(),
        }
    };
    let bind = match bind {
        Ok(b) => b,
        Err(e) => {
            error!(?kind, "bad bind address: {e}");
            return None;
        }
    };
    // Bind, retrying briefly on AddrInUse only — covers the small window when a
    // full-process restart (Windows: spawn-new-then-exit-old) races the old
    // socket's close. Permanent errors (bad/again-privileged address) fail fast.
    let listener = {
        let mut attempt = 0u32;
        loop {
            match TcpListener::bind(bind).await {
                Ok(l) => break l,
                Err(e) if attempt < 5 && e.kind() == std::io::ErrorKind::AddrInUse => {
                    attempt += 1;
                    warn!(?kind, %bind, "address in use (attempt {attempt}); retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                Err(e) => {
                    error!(?kind, %bind, "bind failed: {e}");
                    return None;
                }
            }
        }
    };
    let addr = listener.local_addr().ok()?;

    let router = match kind {
        ServiceKind::Game => data_api::router(state.clone()),
        ServiceKind::Share => share::router(state.clone()),
        ServiceKind::Admin => admin_api::router(state.clone()),
    };

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(async move {
        let svc = router.into_make_service_with_connect_info::<SocketAddr>();
        let serve = axum::serve(listener, svc).with_graceful_shutdown(async move {
            // Wait for the shutdown flag to flip.
            while shutdown_rx.changed().await.is_ok() {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        });
        if let Err(e) = serve.await {
            warn!(?kind, "listener exited: {e}");
        }
    });

    info!(?kind, %addr, "listener up");
    Some((
        Listener {
            kind,
            shutdown: shutdown_tx,
            handle,
        },
        addr,
    ))
}

/// Re-launch this binary in place — the engine behind the admin panel's
/// "restart server" action. Diverges (never returns to the caller).
///
/// Unix: `execv` replaces the process image; the listening sockets are
/// `CLOEXEC`, so the kernel closes them during the exec and the fresh image
/// rebinds cleanly (no port race). Windows: spawn a new process and exit;
/// `spawn_listener`'s short AddrInUse retry absorbs the brief overlap.
///
/// HARD CONSTRAINT #2: only ever called from an explicit, confirmed admin
/// action — never automatically.
pub fn reexec() -> ! {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("blt-server"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    warn!(exe = %exe.display(), ?args, "re-executing blt-server (admin restart)");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // exec() only returns if it failed to replace the image.
        let err = std::process::Command::new(&exe).args(&args).exec();
        error!("re-exec failed: {err}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        match std::process::Command::new(&exe).args(&args).spawn() {
            Ok(_) => std::process::exit(0),
            Err(e) => {
                error!("restart spawn failed: {e}");
                std::process::exit(1);
            }
        }
    }
}
