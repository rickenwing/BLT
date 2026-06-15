//! Localhost media proxy — works around YouTube **Error 153** ("video player
//! configuration error").
//!
//! Since late 2025 YouTube's IFrame player requires the embedding page to send a
//! valid HTTP `Referer`/origin. The app webview runs on the `tauri://localhost`
//! custom protocol, which can't send one, so an embed created directly in the
//! webview is refused (Error 153). Instead we serve a tiny embed page over
//! `http://127.0.0.1:<port>` and load *that* in an iframe — now the player has a
//! real `http` origin and plays. The page reports `ended`/`error` back to the
//! app window via `postMessage` so the jukebox can auto-advance (F9.3).
//!
//! Bound to loopback only — never exposed on the LAN. It serves a single static
//! page parameterised by a sanitised video id; no filesystem or app access.

use crate::state::Shared;
use axum::extract::Query;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use tracing::{info, warn};

#[derive(Deserialize)]
struct YtQuery {
    v: String,
}

/// Start the loopback embed server on an ephemeral port; records it in
/// `ClientState.media_port`. Harmless if it can't bind (YouTube just won't play).
pub async fn start(state: Shared) -> Option<u16> {
    let router = Router::new()
        .route("/ping", get(|| async { "blt" }))
        .route("/yt", get(yt_embed));

    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => {
            warn!("media proxy bind failed (YouTube playback disabled): {e}");
            return None;
        }
    };
    let port = listener.local_addr().ok()?.port();
    *state.media_port.write() = Some(port);
    info!(port, "media proxy (YouTube embed) listening on loopback");
    tauri::async_runtime::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            warn!("media proxy exited: {e}");
        }
    });
    Some(port)
}

/// Serve the YouTube IFrame embed page for a video id. The id is sanitised to
/// the exact character set YouTube uses so it can't break out of the JS string.
async fn yt_embed(Query(q): Query<YtQuery>) -> Html<String> {
    let vid: String =
        q.v.chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .take(16)
            .collect();
    Html(EMBED_PAGE.replace("__VID__", &vid))
}

/// The embed page. `__VID__` is replaced with the sanitised video id. Autoplays,
/// and posts `{blt:"ended"}` / `{blt:"error"}` to the parent window so the
/// jukebox advances on end or on an unplayable video.
const EMBED_PAGE: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="referrer" content="strict-origin-when-cross-origin">
<style>html,body{margin:0;height:100%;background:#000;overflow:hidden}#p,iframe{width:100%;height:100%;border:0;display:block}</style>
</head>
<body>
<div id="p"></div>
<script src="https://www.youtube.com/iframe_api"></script>
<script>
var vid = "__VID__";
function send(m){ try { parent.postMessage({ blt: m }, "*"); } catch (e) {} }
// Best-effort: nudge YouTube toward the highest available quality. YouTube
// deprecated forced quality (~2019) and now picks adaptively from bandwidth +
// player size, so this only biases the initial pick — it can't guarantee 1080p.
function maxQuality(p){
  try {
    var levels = p.getAvailableQualityLevels && p.getAvailableQualityLevels();
    p.setPlaybackQuality(levels && levels.length ? levels[0] : "highres");
  } catch (e) {}
}
window.onYouTubeIframeAPIReady = function () {
  new YT.Player("p", {
    videoId: vid,
    playerVars: { autoplay: 1, rel: 0, playsinline: 1, modestbranding: 1, vq: "hd1080" },
    events: {
      onReady: function (e) { maxQuality(e.target); },
      onError: function () { send("error"); },
      onStateChange: function (e) { if (e.data === 1) maxQuality(e.target); if (e.data === 0) send("ended"); }
    }
  });
};
</script>
</body>
</html>"#;
