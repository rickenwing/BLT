import { useCallback, useEffect, useState } from "react";
import { api, formatBytes, formatSpeed, on, QueueEntry } from "../lib/api";

export default function Downloads() {
  const [queue, setQueue] = useState<QueueEntry[]>([]);

  const load = useCallback(async () => {
    setQueue(await api.downloadsSnapshot());
  }, []);

  useEffect(() => {
    void load();
    const t = setInterval(load, 1500);
    const un = on("downloads-changed", load);
    return () => {
      clearInterval(t);
      void un.then((u) => u());
    };
  }, [load]);

  const active = queue.filter((q) => q.status === "active" || q.status === "pausing");
  const queued = queue.filter((q) => q.status === "queued");
  const rest = queue.filter((q) => !["active", "pausing", "queued"].includes(q.status));

  return (
    <>
      <h1>Downloads</h1>
      {queue.length === 0 && (
        <div className="panel dim">Nothing downloading — grab something from the library.</div>
      )}

      {active.map((q) => (
        <div className="panel" key={`${q.title_id}-${q.manifest_ver}`}>
          <div className="row">
            <strong className="grow">Title #{q.title_id} (v{q.manifest_ver})</strong>
            <span className="dim">{formatSpeed(q.speed_bps)}</span>
            <button onClick={() => api.pauseDownload(q.title_id)}>⏸ Pause</button>
            <button className="danger" onClick={() => api.cancelDownload(q.title_id)}>
              ✕ Cancel
            </button>
          </div>
          <div style={{ margin: "10px 0 6px" }} className="progress">
            <div
              style={{
                width: `${q.total_chunks ? (100 * q.have_chunks) / q.total_chunks : 0}%`,
              }}
            />
          </div>
          <div className="dim">
            {formatBytes(q.bytes_done)} / {formatBytes(q.bytes_total)} ·{" "}
            {q.have_chunks}/{q.total_chunks} chunks
          </div>
          {q.bytes_done > 0 && (
            <div className="dim" style={{ fontSize: 11 }}>
              from server {formatBytes(q.from_server)} · peers{" "}
              {formatBytes(q.from_peers)}
            </div>
          )}
        </div>
      ))}

      {queued.length > 0 && (
        <>
          <h2>Queued (sequential)</h2>
          <div className="panel">
            {queued.map((q, i) => (
              <div className="queue-item" key={`${q.title_id}-${q.manifest_ver}`}>
                <span className="dim">#{i + 1}</span>
                <span className="grow">
                  <strong>{q.name || `Title #${q.title_id}`}</strong>{" "}
                  <span className="dim">→ {q.dest}</span>
                </span>
                <button className="danger" onClick={() => api.cancelDownload(q.title_id)}>
                  Remove
                </button>
              </div>
            ))}
          </div>
        </>
      )}

      {rest.length > 0 && (
        <>
          <h2>History / paused</h2>
          <div className="panel">
            <table>
              <thead>
                <tr>
                  <th>Title</th>
                  <th>Progress</th>
                  <th>Status</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                {rest.map((q) => (
                  <tr key={`${q.title_id}-${q.manifest_ver}`}>
                    <td>
                      Title #{q.title_id} <span className="dim">v{q.manifest_ver}</span>
                      <div className="dim" style={{ fontSize: 11 }}>{q.dest}</div>
                    </td>
                    <td>
                      {q.have_chunks}/{q.total_chunks} chunks
                      {q.error && <div className="error-text">{q.error}</div>}
                    </td>
                    <td>
                      <span
                        className={`badge ${
                          q.status === "complete" ? "ok" : q.status === "error" ? "err" : "warn"
                        }`}
                      >
                        {q.status}
                      </span>
                    </td>
                    <td className="row">
                      {(q.status === "paused" || q.status === "error") && (
                        <button
                          onClick={() =>
                            api
                              .resumeDownload(q.title_id, q.manifest_ver, q.name || `Title #${q.title_id}`)
                              .catch((e) => alert(String(e)))
                          }
                        >
                          ▶ Resume
                        </button>
                      )}
                      <button
                        className="danger"
                        onClick={() => {
                          if (
                            window.confirm(
                              `Remove "${q.name || `Title #${q.title_id}`}" and delete its files from disk?`,
                            )
                          )
                            api
                              .deleteGame(q.title_id)
                              .then(load)
                              .catch((e) => alert(String(e)));
                        }}
                      >
                        🗑 Remove
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
    </>
  );
}
