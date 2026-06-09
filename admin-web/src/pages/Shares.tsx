import { useCallback, useEffect, useState } from "react";
import { api, formatBytes, formatWhen, ShareSummary } from "../api";

export default function Shares() {
  const [shares, setShares] = useState<ShareSummary[]>([]);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setShares(await api.get<ShareSummary[]>("/api/shares"));
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

  async function remove(s: ShareSummary) {
    // Confirmation gate — deletes are destructive (HARD CONSTRAINT #5).
    if (
      !window.confirm(
        `Delete "${s.name}" (${formatBytes(s.size)}, uploaded by ${s.owner_name})? This removes the files from the share drive.`,
      )
    ) {
      return;
    }
    await api.delete(`/api/shares/${s.id}`);
    await load();
  }

  return (
    <>
      <h1>Shared Pool</h1>
      {error && <div className="error-text">{error}</div>}
      <div className="panel">
        {shares.length === 0 ? (
          <p className="dim">
            Nothing here yet. Players upload files and folders from their
            clients; as admin you can moderate (delete) anything here.
          </p>
        ) : (
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Kind</th>
                <th>Size</th>
                <th>Files</th>
                <th>Owner</th>
                <th>Uploaded</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {shares.map((s) => (
                <tr key={s.id}>
                  <td>
                    <strong>{s.name}</strong>
                  </td>
                  <td>
                    <span className="badge">{s.kind}</span>
                  </td>
                  <td>{formatBytes(s.size)}</td>
                  <td>{s.file_count}</td>
                  <td>{s.owner_name}</td>
                  <td className="dim">{formatWhen(s.created_at)}</td>
                  <td>
                    <button className="danger" onClick={() => remove(s)}>
                      Delete
                    </button>
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
