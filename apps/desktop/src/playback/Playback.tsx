import { useCallback, useEffect, useRef, useState } from "react";
import { api, AppBootState, JukeboxItem, JukeboxState, on } from "../lib/api";
import { getCurrentWindow } from "@tauri-apps/api/window";

/** Extract a YouTube video id from the usual URL shapes. */
function youtubeId(url: string): string | null {
  const m =
    url.match(/[?&]v=([\w-]{6,})/) ||
    url.match(/youtu\.be\/([\w-]{6,})/) ||
    url.match(/youtube\.com\/embed\/([\w-]{6,})/) ||
    url.match(/youtube\.com\/shorts\/([\w-]{6,})/);
  return m ? m[1] : null;
}

/** A short, readable label for a jukebox item: its title, else a friendly form
 *  of the ref (YouTube id, or the host of a URL) instead of a giant raw URL. */
function prettyRef(title: string | null | undefined, ref: string): string {
  if (title) return title;
  const yt = youtubeId(ref);
  if (yt) return `YouTube · ${yt}`;
  try {
    return new URL(ref).hostname.replace(/^www\./, "");
  } catch {
    return ref;
  }
}

export default function Playback({ boot }: { boot: AppBootState }) {
  const [state, setState] = useState<JukeboxState | null>(null);
  const [unlockPw, setUnlockPw] = useState("");
  const [showUnlock, setShowUnlock] = useState(false);
  const launchedFor = useRef<number | null>(null);

  const load = useCallback(async () => {
    setState(await api.jukeboxState());
  }, []);

  useEffect(() => {
    void load();
    const un = on("jukebox-changed", load);
    return () => {
      void un.then((u) => u());
    };
  }, [load]);

  // External/DRM lane (F10.1-.2): when an external item becomes current, open
  // it ONCE in the real browser; the queue then waits for a human Next.
  useEffect(() => {
    const np = state?.now_playing;
    if (
      np &&
      np.type === "external" &&
      state?.playback_state === "playing_external" &&
      launchedFor.current !== np.id
    ) {
      launchedFor.current = np.id;
      void api.externalOpen(np.ref).catch((e) => console.error("external open:", e));
    }
  }, [state]);

  async function next() {
    await api.jukeboxNext().catch((e) => alert(String(e)));
  }

  async function unlock() {
    try {
      await api.lockdownExit(unlockPw);
    } catch (e) {
      alert(String(e));
    }
  }

  async function toggleFullscreen() {
    const win = getCurrentWindow();
    const is = await win.isFullscreen();
    await win.setFullscreen(!is);
  }

  const np = state?.now_playing ?? null;

  return (
    <div className="playback">
      <div className="stage">
        {!np && (
          <div className="external-card dim">
            <div className="big">
              <span style={{ color: "var(--accent)" }}>BLT</span> Jukebox
            </div>
            Queue is empty — add videos from any client.
          </div>
        )}
        {np && np.type === "youtube" && <YouTubePlayer key={np.id} item={np} />}
        {np && (np.type === "direct_url" || np.type === "shared_file") && (
          <StreamPlayer key={np.id} item={np} />
        )}
        {np && np.type === "external" && (
          <div className="external-card">
            <div className="big">▶ Playing externally</div>
            <p className="dim">
              “{np.title || np.ref}” opened in the browser.
              <br />
              Press <strong>Next</strong> when it's done — the queue is waiting.
            </p>
            <button onClick={() => api.externalOpen(np.ref)}>Open again</button>
          </div>
        )}
      </div>

      <div className="bar">
        <div className="grow">
          {np ? (
            <div className="np-line">
              <strong>{prettyRef(np.title, np.ref)}</strong>{" "}
              <span className="dim">added by {np.added_by}</span>
            </div>
          ) : (
            <span className="dim">Nothing playing</span>
          )}
          {state && state.up_next.length > 0 && (
            <div className="upnext-strip">
              {state.up_next.slice(0, 6).map((i) => (
                <span className="card" key={i.id} title={i.title || i.ref}>
                  ▲{i.votes} {prettyRef(i.title, i.ref)}
                </span>
              ))}
            </div>
          )}
        </div>
        <button className="primary" onClick={next}>
          ⏭ Next
        </button>
        <button onClick={toggleFullscreen}>⛶</button>
        {boot.settings.playback_locked ? (
          <button onClick={() => setShowUnlock(true)}>🔒</button>
        ) : (
          <button onClick={() => setShowUnlock(true)}>Exit playback…</button>
        )}
      </div>

      {showUnlock && (
        <div className="modal-backdrop" onClick={() => setShowUnlock(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h3>Exit playback mode</h3>
            <p className="dim">
              The admin password is required to take this machine out of
              playback mode (F14).
            </p>
            <div className="row">
              <input
                type="password"
                placeholder="Admin password"
                value={unlockPw}
                onChange={(e) => setUnlockPw(e.target.value)}
                autoFocus
              />
              <button className="primary" onClick={unlock}>
                Unlock
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

/**
 * Embedded YouTube. The player runs inside a tiny page served over
 * `http://127.0.0.1:<port>` by the Rust media proxy, NOT directly in the webview
 * — `tauri://localhost` can't send a valid HTTP Referer, which YouTube now
 * rejects with Error 153. The localhost page gives it a real http origin. It
 * posts `{blt:"ended"|"error"}` back here so the jukebox auto-advances (F9.3).
 */
function YouTubePlayer({ item }: { item: JukeboxItem }) {
  const vid = youtubeId(item.ref);
  const [port, setPort] = useState<number | null>(null);
  const [portError, setPortError] = useState(false);

  useEffect(() => {
    let live = true;
    api
      .mediaProxyPort()
      .then((p) => {
        if (!live) return;
        if (p) setPort(p);
        else setPortError(true);
      })
      .catch(() => live && setPortError(true));
    return () => {
      live = false;
    };
  }, []);

  // Advance on end / unplayable video, reported by the localhost embed page.
  useEffect(() => {
    function onMessage(e: MessageEvent) {
      const m = (e.data && (e.data as { blt?: string }).blt) || null;
      if (m === "ended" || m === "error") void api.jukeboxEnded();
    }
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, []);

  if (!vid) {
    return (
      <div className="external-card">
        <div className="big">Unplayable YouTube link</div>
        <p className="dim">{item.ref}</p>
      </div>
    );
  }
  if (portError) {
    return (
      <div className="external-card">
        <div className="big">Couldn't start the YouTube player</div>
        <p className="dim">
          The local media helper isn't available. Restart the app, then press
          Next.
        </p>
        <p className="dim">{item.ref}</p>
      </div>
    );
  }
  if (port == null) return <div className="external-card dim">Loading…</div>;
  return (
    <iframe
      key={vid}
      src={`http://127.0.0.1:${port}/yt?v=${encodeURIComponent(vid)}`}
      allow="autoplay; encrypted-media; fullscreen"
      allowFullScreen
      style={{ width: "100%", height: "100%", border: 0 }}
    />
  );
}

/** Direct URLs + shared-pool files stream straight into an HTML5 video
 * element (Range requests against the share service — F9.2). */
function StreamPlayer({ item }: { item: JukeboxItem }) {
  const [src, setSrc] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  // null = still deciding; true = embed mpv; false = fall back to HTML5 <video>.
  const [useMpv, setUseMpv] = useState<boolean | null>(null);

  useEffect(() => {
    if (item.type === "direct_url") {
      setSrc(item.ref);
    } else {
      void api
        .resolveShareStream(Number(item.ref))
        .then(setSrc)
        .catch((e) => setErr(String(e)));
    }
  }, [item]);

  // Prefer mpv for arbitrary media (HEVC/MKV/FLAC the webview can't decode);
  // fall back to the HTML5 player when mpv isn't installed.
  useEffect(() => {
    void api.mpvAvailable().then(setUseMpv);
  }, []);

  // Drive the embedded mpv player: load on src, stop on unmount/skip.
  useEffect(() => {
    if (!src || useMpv !== true) return;
    api.mpvLoad(src).catch((e) => setErr(String(e)));
    return () => void api.mpvStop();
  }, [src, useMpv]);

  // mpv exiting at end-of-file advances the jukebox (F9.3); a failure surfaces
  // an error (with mpv's message) instead of silently skipping.
  useEffect(() => {
    if (useMpv !== true) return;
    const subs = [
      on("mpv-ended", () => void api.jukeboxEnded()),
      on("mpv-failed", (e) =>
        setErr(
          typeof e.payload === "string" && e.payload
            ? `mpv: ${e.payload}`
            : "mpv playback failed",
        ),
      ),
    ];
    return () => subs.forEach((s) => void s.then((u) => u()));
  }, [useMpv]);

  if (err) {
    return (
      <div className="external-card">
        <div className="big">Can't play this item</div>
        <p className="dim">{err}</p>
        <button onClick={() => api.jukeboxEnded()}>Skip</button>
      </div>
    );
  }
  if (!src || useMpv === null) {
    return <div className="external-card dim">Resolving stream…</div>;
  }
  if (useMpv) {
    // mpv renders embedded into the window; this is the backdrop while it loads.
    return <div className="external-card dim">▶ Playing via mpv…</div>;
  }
  return (
    <video
      src={src}
      autoPlay
      controls
      onEnded={() => void api.jukeboxEnded()}
      onError={() => setErr("playback failed (codec or connection)")}
    />
  );
}
