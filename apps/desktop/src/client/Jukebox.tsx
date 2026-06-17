import { FormEvent, useCallback, useEffect, useState } from "react";
import { api, confirmDialog, ItemType, JukeboxState, notify, on, ShareSummary } from "../lib/api";

const TYPE_LABELS: Record<ItemType, string> = {
  youtube: "YouTube",
  direct_url: "Direct URL",
  shared_file: "Shared file",
  external: "External / DRM",
};

const VIDEO_EXT = ["mp4", "webm", "mkv", "mov", "m4v", "avi", "ogv"];

export default function Jukebox() {
  const [state, setState] = useState<JukeboxState | null>(null);
  const [addType, setAddType] = useState<ItemType>("youtube");
  const [addRef, setAddRef] = useState("");
  const [addTitle, setAddTitle] = useState("");
  const [shares, setShares] = useState<ShareSummary[]>([]);

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

  useEffect(() => {
    if (addType === "shared_file") {
      void api
        .sharesList()
        .then((all) =>
          setShares(
            all.filter(
              (s) =>
                s.kind === "file" ||
                // folder shares can hold videos too; allow picking any share
                s.kind === "folder",
            ),
          ),
        )
        .catch(() => setShares([]));
    }
  }, [addType]);

  async function add(e: FormEvent) {
    e.preventDefault();
    if (!addRef.trim()) return;
    // External/DRM items will open a browser on the playback machine — the
    // explicit confirmation lives here in the initiating UI (#5 / F10).
    if (addType === "external") {
      const ok = await confirmDialog(
        "External links (Netflix/Hulu/Prime…) open in the playback machine's real browser " +
          "and wait for a human to press Next. Add it?",
      );
      if (!ok) return;
    }
    try {
      await api.jukeboxAdd(addType, addRef.trim(), addTitle.trim() || null);
      setAddRef("");
      setAddTitle("");
    } catch (e2) {
      void notify(String(e2));
    }
  }

  if (!state) {
    return (
      <>
        <h1>Jukebox</h1>
        <div className="panel dim">Waiting for the live channel…</div>
      </>
    );
  }

  return (
    <>
      <h1>Jukebox</h1>

      <h2>Now playing</h2>
      <div className="panel">
        {state.now_playing ? (
          <>
            <strong>{state.now_playing.title || state.now_playing.ref}</strong>{" "}
            <span className="badge">{TYPE_LABELS[state.now_playing.type]}</span>
            <div className="dim">added by {state.now_playing.added_by}</div>
            {state.playback_state === "playing_external" && (
              <div className="badge warn" style={{ marginTop: 8 }}>
                ▶ Playing externally on the TV — the playback machine (or admin)
                presses Next
              </div>
            )}
          </>
        ) : (
          <span className="dim">Silence… add something!</span>
        )}
      </div>

      <h2>Up next ({state.up_next.length}) — {state.mode === "fair" ? "fair rotation" : "vote-ranked"}</h2>
      <div className="panel">
        {state.up_next.length === 0 ? (
          <span className="dim">Queue is empty.</span>
        ) : (
          state.up_next.map((item) => (
            <div className="queue-item" key={item.id}>
              <div
                className={`votes ${item.voted_by_me ? "mine" : ""}`}
                title={item.voted_by_me ? "remove your upvote" : "upvote"}
                onClick={() => api.jukeboxVote(item.id)}
              >
                ▲{item.votes}
              </div>
              <div className="grow">
                <strong>{item.title || item.ref}</strong>{" "}
                <span className="badge">{TYPE_LABELS[item.type]}</span>
                <div className="dim">added by {item.added_by}</div>
              </div>
            </div>
          ))
        )}
      </div>

      <h2>Add to queue</h2>
      <form className="panel" onSubmit={add}>
        <div className="row wrap">
          <select
            value={addType}
            onChange={(e) => {
              setAddType(e.target.value as ItemType);
              setAddRef("");
            }}
            style={{ width: 150 }}
          >
            {Object.entries(TYPE_LABELS).map(([k, v]) => (
              <option key={k} value={k}>
                {v}
              </option>
            ))}
          </select>
          {addType === "shared_file" ? (
            // Pick from the shared pool rather than typing an id (F8.2).
            <select
              className="grow"
              value={addRef}
              onChange={(e) => {
                setAddRef(e.target.value);
                const s = shares.find((x) => String(x.id) === e.target.value);
                if (s && !addTitle) setAddTitle(s.name);
              }}
            >
              <option value="">— pick a shared video —</option>
              {shares
                .filter(
                  (s) =>
                    s.kind === "folder" ||
                    VIDEO_EXT.some((ext) => s.name.toLowerCase().endsWith(`.${ext}`)),
                )
                .map((s) => (
                  <option key={s.id} value={String(s.id)}>
                    {s.name} ({s.owner_name})
                  </option>
                ))}
            </select>
          ) : (
            <input
              className="grow"
              placeholder={addType === "youtube" ? "YouTube URL" : "URL"}
              value={addRef}
              onChange={(e) => setAddRef(e.target.value)}
            />
          )}
          <input
            placeholder="Title (optional)"
            style={{ width: 180 }}
            value={addTitle}
            onChange={(e) => setAddTitle(e.target.value)}
          />
          <button className="primary">Add</button>
        </div>
      </form>
      <p className="dim">
        Upvote-only — the most-wanted videos rise. Only the playback machine
        plays audio/video; this screen is for queueing and voting.
      </p>
    </>
  );
}
