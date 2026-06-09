import { useCallback, useEffect, useState } from "react";
import { api, formatBytes, formatWhen, ScanSummary, TitleRow } from "../api";

export default function Titles() {
  const [titles, setTitles] = useState<TitleRow[]>([]);
  const [scanning, setScanning] = useState(false);
  const [summary, setSummary] = useState<ScanSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<number | null>(null);
  const [labelDraft, setLabelDraft] = useState("");

  const load = useCallback(async () => {
    try {
      setTitles(await api.get<TitleRow[]>("/api/titles"));
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : "failed");
    }
  }, []);

  useEffect(() => {
    void load();
    const t = setInterval(load, 10000);
    return () => clearInterval(t);
  }, [load]);

  async function scanNow() {
    setScanning(true);
    setSummary(null);
    setError(null);
    try {
      setSummary(await api.post<ScanSummary>("/api/scan"));
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : "scan failed");
    } finally {
      setScanning(false);
    }
  }

  async function saveLabel(id: number) {
    await api.put(`/api/titles/${id}/label`, { label: labelDraft || null });
    setEditing(null);
    await load();
  }

  return (
    <>
      <div className="row">
        <h1 className="grow">Game Library</h1>
        <button className="primary" onClick={scanNow} disabled={scanning}>
          {scanning ? "Scanning…" : "Scan now"}
        </button>
      </div>

      {error && <div className="error-text">{error}</div>}
      {summary && (
        <div className="success-text">
          Scanned {summary.scanned} — published {summary.published.length},
          updated {summary.republished.length}, info-only{" "}
          {summary.info_updated.length}, removed {summary.removed.length}
          {summary.errors.length > 0 && (
            <span className="error-text"> — {summary.errors.length} errors</span>
          )}
        </div>
      )}

      <div className="panel">
        {titles.length === 0 ? (
          <p className="dim">
            Nothing here yet. Set a library path in Settings, drop game folders
            in (or stage them), then Scan now.
          </p>
        ) : (
          <table>
            <thead>
              <tr>
                <th>Title</th>
                <th>Version</th>
                <th>Files</th>
                <th>Size</th>
                <th>State</th>
                <th>Meta</th>
                <th>Last scan</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {titles.map((t) => (
                <tr key={t.id}>
                  <td>
                    {editing === t.id ? (
                      <span className="row">
                        <input
                          value={labelDraft}
                          onChange={(e) => setLabelDraft(e.target.value)}
                          placeholder={t.name}
                          autoFocus
                        />
                        <button className="primary" onClick={() => saveLabel(t.id)}>
                          Save
                        </button>
                        <button onClick={() => setEditing(null)}>✕</button>
                      </span>
                    ) : (
                      <>
                        <strong>{t.label || t.name}</strong>
                        {t.label && <span className="dim"> ({t.name})</span>}
                      </>
                    )}
                  </td>
                  <td>v{t.manifest_ver}</td>
                  <td>{t.file_count}</td>
                  <td>{formatBytes(t.total_size)}</td>
                  <td>
                    <span
                      className={`badge ${t.state === "published" ? "ok" : "warn"}`}
                    >
                      {t.state}
                    </span>
                  </td>
                  <td>
                    {t.has_metadata && <span className="badge">info</span>}{" "}
                    {t.has_cover && <span className="badge">cover</span>}{" "}
                    {t.has_install_script && <span className="badge warn">script</span>}
                  </td>
                  <td className="dim">{formatWhen(t.last_scan)}</td>
                  <td>
                    {editing !== t.id && (
                      <button
                        onClick={() => {
                          setEditing(t.id);
                          setLabelDraft(t.label ?? "");
                        }}
                      >
                        Rename
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </>
  );
}
