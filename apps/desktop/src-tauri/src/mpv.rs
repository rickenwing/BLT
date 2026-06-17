//! Embedded mpv playback for local/LAN media the webview's HTML5 `<video>` can't
//! decode (HEVC/10-bit, MKV, FLAC/DTS, etc.).
//!
//! We drive the **system mpv binary** (rather than linking libmpv — no
//! build-time dependency, just a runtime requirement that mpv is installed).
//!
//! DESIGN-NOTE: the first attempt embedded mpv *into the webview's own view* via
//! `--wid`, which broke WKWebView's rendering (white screen). True in-window
//! embedding needs a separate sibling NSView created on the main thread — a lot
//! of blind native code. For now we run mpv as its own **fullscreen window over
//! the app**, which never touches the webview and is the same end result for a
//! jukebox (full-screen video, any codec). Each item spawns mpv on the resolved
//! URL; mpv exiting (EOF, or we kill it to load the next) is seen by a watcher
//! thread, and a generation counter distinguishes a natural EOF (→ advance the
//! jukebox, F9.3) from a superseded player (a newer load / a stop — no advance).

use crate::state::Shared;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tauri::{Emitter, Manager, WebviewWindow};
use tracing::{info, warn};

/// One-player state: just the current generation. The child is owned by its
/// watcher thread, which polls this to know when it has been superseded.
#[derive(Default)]
pub struct MpvPlayer {
    generation: u64,
}

/// Locate a usable mpv binary. A `.app` launched from Finder gets a minimal PATH
/// without Homebrew/MacPorts, so probe common absolute locations too — not just
/// PATH. DESIGN-NOTE: a Settings-configurable path can be added later.
fn mpv_path() -> Option<PathBuf> {
    let candidates: &[&str] = &[
        "/opt/homebrew/bin/mpv", // macOS Apple Silicon (Homebrew)
        "/usr/local/bin/mpv",    // macOS Intel / common
        "/opt/local/bin/mpv",    // MacPorts
        "mpv",                   // PATH (shell launches, Windows)
        "mpv.exe",
    ];
    candidates.iter().map(PathBuf::from).find(|p| {
        Command::new(p)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// Is a usable mpv available? Drives the client's fallback to the HTML5 player.
pub fn available() -> bool {
    mpv_path().is_some()
}

/// Play `url` in a fullscreen mpv window over the app. Supersedes (and kills)
/// any current player. `window` is kept for the app handle (event emit).
pub fn load(state: &Shared, window: &WebviewWindow, url: &str) -> Result<(), String> {
    let bin = mpv_path().ok_or("mpv not found — install it (e.g. `brew install mpv`)")?;
    let app = window.app_handle().clone();

    // Route the stream through our loopback proxy so mpv talks to 127.0.0.1
    // (exempt from macOS Local Network privacy) instead of the LAN directly — a
    // spawned helper like mpv is otherwise blocked from the LAN under a GUI
    // launch, even though the app itself has access. Fall back to direct if the
    // proxy isn't up.
    let play_url = match *state.media_port.read() {
        Some(port) => format!(
            "http://127.0.0.1:{port}/stream?u={}",
            crate::server_api::urlencode(url)
        ),
        None => url.to_string(),
    };
    info!(%play_url, "mpv play");

    let generation = {
        let mut p = state.mpv.lock();
        p.generation += 1;
        p.generation
    };

    let mut child = Command::new(&bin)
        .args([
            "--no-terminal".into(),
            "--really-quiet".into(),
            // Quit at end of file so the watcher sees the exit and advances.
            "--keep-open=no".into(),
            "--idle=no".into(),
            // Fullscreen window over the app (no webview contact). mpv keeps its
            // own OSC + keybindings (e.g. `q` to skip, space to pause) for now.
            "--fullscreen=yes".into(),
            "--ontop=yes".into(),
            play_url,
        ])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not launch mpv ({e}) — is it installed?"))?;

    let state = state.clone();
    std::thread::spawn(move || {
        loop {
            // Superseded by a newer load() or a stop()? Kill this player, no advance.
            if state.mpv.lock().generation != generation {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        // Natural end-of-file (or `q`) → advance the jukebox.
                        let _ = app.emit("mpv-ended", ());
                    } else {
                        // mpv couldn't play it — surface the error instead of silently
                        // skipping, and log its stderr so failures are diagnosable.
                        let mut err = String::new();
                        if let Some(mut e) = child.stderr.take() {
                            use std::io::Read;
                            let _ = e.read_to_string(&mut err);
                        }
                        let err = err.trim().to_string();
                        warn!(code = status.code(), stderr = %err, "mpv exited with error");
                        let _ = app.emit("mpv-failed", err);
                    }
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(250)),
                Err(_) => return,
            }
        }
    });
    Ok(())
}

/// Stop the current player without advancing (supersedes it; its watcher kills
/// the child on the next poll).
pub fn stop(state: &Shared) {
    state.mpv.lock().generation += 1;
}
