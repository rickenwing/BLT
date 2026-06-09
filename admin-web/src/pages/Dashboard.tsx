import { useEffect, useState } from "react";
import { api, formatUptime, Status } from "../api";

export default function Dashboard() {
  const [status, setStatus] = useState<Status | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    const load = async () => {
      try {
        const s = await api.get<Status>("/api/status");
        if (live) {
          setStatus(s);
          setError(null);
        }
      } catch (e) {
        if (live) setError(e instanceof Error ? e.message : "failed");
      }
    };
    void load();
    const t = setInterval(load, 5000);
    return () => {
      live = false;
      clearInterval(t);
    };
  }, []);

  if (error) return <div className="error-text">Server unreachable: {error}</div>;
  if (!status) return <div className="dim">Loading…</div>;

  const pathBadge = (p: string | null) =>
    p ? <span className="badge ok">{p}</span> : <span className="badge warn">not set</span>;

  return (
    <>
      <h1>{status.label}</h1>
      <div className="cards">
        <div className="card">
          <div className="label">Version</div>
          <div className="value">{status.version}</div>
        </div>
        <div className="card">
          <div className="label">Uptime</div>
          <div className="value">{formatUptime(status.uptime_secs)}</div>
        </div>
        <div className="card">
          <div className="label">Connected clients</div>
          <div className="value">{status.connections}</div>
        </div>
      </div>

      <h2>Service bindings</h2>
      <div className="panel">
        <table>
          <tbody>
            <tr>
              <td>Game distribution</td>
              <td>
                <code>{status.binds.game_distribution}</code>
              </td>
            </tr>
            <tr>
              <td>Shared pool</td>
              <td>
                <code>{status.binds.shared_pool}</code>
              </td>
            </tr>
            <tr>
              <td>Admin panel</td>
              <td>
                <code>{status.binds.admin_panel}</code>
              </td>
            </tr>
          </tbody>
        </table>
      </div>

      <h2>Storage paths</h2>
      <div className="panel">
        <table>
          <tbody>
            <tr>
              <td>Game library</td>
              <td>{pathBadge(status.paths.library)}</td>
            </tr>
            <tr>
              <td>Staging</td>
              <td>{pathBadge(status.paths.staging)}</td>
            </tr>
            <tr>
              <td>Shared pool</td>
              <td>{pathBadge(status.paths.share)}</td>
            </tr>
          </tbody>
        </table>
        <p className="dim" style={{ marginBottom: 0 }}>
          Configure paths under Settings. Titles publish automatically once the
          library path is set and scanned.
        </p>
      </div>
    </>
  );
}
