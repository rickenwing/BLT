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
            <>
              <strong>{np.title || np.ref}</strong>{" "}
              <span className="dim">added by {np.added_by}</span>
            </>
          ) : (
            <span className="dim">Nothing playing</span>
          )}
          {state && state.up_next.length > 0 && (
            <div className="upnext-strip">
              {state.up_next.slice(0, 6).map((i) => (
                <span className="card" key={i.id}>
                  ▲{i.votes} {i.title || i.ref}
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

/** Embedded YouTube via the IFrame API; `ended` → auto-advance (F9.3). */
declare global {
  interface Window {
    YT?: any;
    onYouTubeIframeAPIReady?: () => void;
  }
}

function YouTubePlayer({ item }: { item: JukeboxItem }) {
  const holder = useRef<HTMLDivElement>(null);
  const vid = youtubeId(item.ref);
  const [loadError, setLoadError] = useState(false);

  useEffect(() => {
    if (!vid || !holder.current) return;
    let player: any;
    let cancelled = false;

    // YT.Player REPLACES its target element with an <iframe>. Mount it on a
    // throwaway child div — NOT the React-managed holder — so React re-renders
    // (the playback view re-renders on every jukebox WS broadcast) don't fight
    // YT over the same DOM node and blow the iframe away, which left the stage
    // blank after Play.
    const mount = document.createElement("div");
    holder.current.innerHTML = "";
    holder.current.appendChild(mount);

    function create() {
      if (cancelled) return;
      player = new window.YT.Player(mount, {
        videoId: vid,
        playerVars: { autoplay: 1, rel: 0 },
        events: {
          onReady: () => setLoadError(false),
          onStateChange: (e: { data: number }) => {
            if (e.data === 0 /* ended */) {
              void api.jukeboxEnded();
            }
          },
          onError: () => {
            // Unplayable video: advance rather than stalling the party.
            void api.jukeboxEnded();
          },
        },
      });
    }

    if (window.YT?.Player) {
      create();
    } else {
      const tag = document.createElement("script");
      tag.src = "https://www.youtube.com/iframe_api";
      tag.onerror = () => setLoadError(true);
      document.head.appendChild(tag);
      const prev = window.onYouTubeIframeAPIReady;
      window.onYouTubeIframeAPIReady = () => {
        prev?.();
        create();
      };
    }
    // No blank screens: if the IFrame API never initialises (no internet /
    // blocked), surface it so the operator knows why instead of staring at a
    // black stage.
    const watchdog = window.setTimeout(() => {
      if (!cancelled && !window.YT?.Player) setLoadError(true);
    }, 12000);

    return () => {
      cancelled = true;
      window.clearTimeout(watchdog);
      try {
        player?.destroy();
      } catch {
        /* ignore */
      }
    };
  }, [vid]);

  if (!vid) {
    return (
      <div className="external-card">
        <div className="big">Unplayable YouTube link</div>
        <p className="dim">{item.ref}</p>
      </div>
    );
  }
  if (loadError) {
    return (
      <div className="external-card">
        <div className="big">Couldn't load YouTube</div>
        <p className="dim">
          This machine needs internet to play YouTube. Check its connection,
          then press Next.
        </p>
        <p className="dim">{item.ref}</p>
      </div>
    );
  }
  return <div ref={holder} style={{ width: "100%", height: "100%" }} />;
}

/** Direct URLs + shared-pool files stream straight into an HTML5 video
 * element (Range requests against the share service — F9.2). */
function StreamPlayer({ item }: { item: JukeboxItem }) {
  const [src, setSrc] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

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

  if (err) {
    return (
      <div className="external-card">
        <div className="big">Can't stream this share</div>
        <p className="dim">{err}</p>
        <button onClick={() => api.jukeboxEnded()}>Skip</button>
      </div>
    );
  }
  if (!src) return <div className="external-card dim">Resolving stream…</div>;
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
