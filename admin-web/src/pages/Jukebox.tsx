import { FormEvent, useCallback, useEffect, useState } from "react";
import { api, JukeboxItem, JukeboxState } from "../api";

const TYPE_LABELS: Record<JukeboxItem["type"], string> = {
  youtube: "YouTube",
  direct_url: "Direct URL",
  shared_file: "Shared file",
  external: "External / DRM",
};

export default function Jukebox() {
  const [state, setState] = useState<JukeboxState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [addType, setAddType] = useState<JukeboxItem["type"]>("youtube");
  const [addRef, setAddRef] = useState("");
  const [addTitle, setAddTitle] = useState("");

  const load = useCallback(async () => {
    try {
      setState(await api.get<JukeboxState>("/api/jukebox"));
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : "failed");
    }
  }, []);

  useEffect(() => {
    void load();
    const t = setInterval(load, 3000);
    return () => clearInterval(t);
  }, [load]);

  async function addItem(e: FormEvent) {
    e.preventDefault();
    if (!addRef.trim()) return;
    await api.post("/api/jukebox/items", {
      type: addType,
      ref: addRef.trim(),
      title: addTitle.trim() || null,
    });
    setAddRef("");
    setAddTitle("");
    await load();
  }

  async function next() {
    await api.post("/api/jukebox/next");
    await load();
  }

  async function clearQueue() {
    if (!window.confirm("Clear the whole queue?")) return;
    await api.post("/api/jukebox/clear");
    await load();
  }

  async function setMode(mode: string) {
    await api.put("/api/jukebox/mode", { mode });
    await load();
  }

  async function remove(id: number) {
    await api.delete(`/api/jukebox/items/${id}`);
    await load();
  }

  async function toTop(id: number) {
    await api.post(`/api/jukebox/items/${id}/top`);
    await load();
  }

  if (error) return <div className="error-text">{error}</div>;
  if (!state) return <div className="dim">Loading…</div>;

  return (
    <>
      <div className="row">
        <h1 className="grow">Jukebox</h1>
        <select
          value={state.mode}
          onChange={(e) => setMode(e.target.value)}
          style={{ width: "auto" }}
          title="Up-next ordering mode"
        >
          <option value="fair">Fair rotation</option>
          <option value="votes">Vote-ranked</option>
        </select>
        <button className="primary" onClick={next}>
          ⏭ Next
        </button>
        <button className="danger" onClick={clearQueue}>
          Clear queue
        </button>
      </div>

      <h2>Now playing</h2>
      <div className="panel np">
        {state.now_playing ? (
          <>
            <strong>{state.now_playing.title || state.now_playing.ref}</strong>{" "}
            <span className="badge">{TYPE_LABELS[state.now_playing.type]}</span>
            <div className="dim">added by {state.now_playing.added_by}</div>
            {state.playback_state === "playing_external" && (
              <div className="badge warn" style={{ marginTop: 8 }}>
                ▶ Playing externally — press Next to continue
              </div>
            )}
          </>
        ) : (
          <span className="dim">Nothing playing.</span>
        )}
      </div>

      <h2>Up next ({state.up_next.length})</h2>
      <div className="panel">
        {state.up_next.length === 0 ? (
          <span className="dim">Queue is empty — add something below.</span>
        ) : (
          state.up_next.map((item) => (
            <div className="queue-item" key={item.id}>
              <div className="votes">▲{item.votes}</div>
              <div className="grow">
                <strong>{item.title || item.ref}</strong>{" "}
                <span className="badge">{TYPE_LABELS[item.type]}</span>
                <div className="dim">added by {item.added_by}</div>
              </div>
              <button onClick={() => toTop(item.id)} title="Move to top">
                ⤒ Top
              </button>
              <button className="danger" onClick={() => remove(item.id)}>
                Remove
              </button>
            </div>
          ))
        )}
      </div>

      <h2>Add item</h2>
      <form className="panel" onSubmit={addItem}>
        <div className="row wrap">
          <select
            value={addType}
            onChange={(e) => setAddType(e.target.value as JukeboxItem["type"])}
            style={{ width: 160 }}
          >
            {Object.entries(TYPE_LABELS).map(([k, v]) => (
              <option key={k} value={k}>
                {v}
              </option>
            ))}
          </select>
          <input
            className="grow"
            placeholder={
              addType === "shared_file"
                ? "share id (pick from the client UI normally)"
                : "URL"
            }
            value={addRef}
            onChange={(e) => setAddRef(e.target.value)}
          />
          <input
            placeholder="Title (optional)"
            style={{ width: 200 }}
            value={addTitle}
            onChange={(e) => setAddTitle(e.target.value)}
          />
          <button className="primary">Add</button>
        </div>
      </form>
    </>
  );
}
