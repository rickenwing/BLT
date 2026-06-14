import { useEffect, useState } from "react";
import { api, formatUptime, Status } from "../api";

interface UpdateInfo {
  current: string;
  latest: string | null;
  update_available: boolean;
  notes: string;
  url: string;
}

export default function Dashboard() {
  const [status, setStatus] = useState<Status | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [actionMsg, setActionMsg] = useState<string | null>(null);
  const [update, setUpdate] = useState<UpdateInfo | null>(null);

  useEffect(() => {
    void (async () => {
      try {
        setUpdate(await api.get<UpdateInfo>("/api/update/check"));
      } catch {
        /* offline / rate-limited — leave the banner hidden */
      }
    })();
  }, []);

  async function installUpdate() {
    if (
      !confirm(
        `Download server ${update?.latest} and restart now? Connected clients drop briefly and reconnect; downloads resume.`,
      )
    )
      return;
    setActionMsg("Downloading & installing the update — the server will restart…");
    try {
      await api.post("/api/update/install");
    } catch (e) {
      setActionMsg(e instanceof Error ? `Update failed: ${e.message}` : "failed");
    }
  }

  async function restartService(kind: string, label: string) {
    const adminWarn =
      kind === "admin"
        ? "\n\nThis briefly disconnects the admin panel — it reconnects on the same address."
        : "";
    if (
      !confirm(
        `Restart the ${label} service? In-flight requests finish first; clients reconnect automatically and downloads resume.${adminWarn}`,
      )
    )
      return;
    setActionMsg(null);
    try {
      await api.post(`/api/services/${kind}/restart`);
      setActionMsg(`${label} service restart requested.`);
    } catch (e) {
      setActionMsg(e instanceof Error ? `Failed: ${e.message}` : "failed");
    }
  }

  async function restartServer() {
    if (
      !confirm(
        "Restart the entire server process? All connected clients drop for ~1–2s and reconnect; downloads resume from where they left off.",
      )
    )
      return;
    setActionMsg(null);
    try {
      await api.post("/api/server/restart");
      setActionMsg("Server is restarting — this page will reconnect shortly…");
    } catch (e) {
      setActionMsg(e instanceof Error ? `Failed: ${e.message}` : "failed");
    }
  }

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

      {update?.update_available && (
        <div className="update-banner">
          <strong>Server update available: {update.latest}</strong>{" "}
          <span className="dim">(you're on {update.current})</span>
          {update.notes && <pre>{update.notes}</pre>}
          <div style={{ marginTop: 10 }}>
            <button className="primary" onClick={installUpdate}>
              ↻ Download &amp; restart
            </button>
            {update.url && (
              <a
                href={update.url}
                target="_blank"
                rel="noreferrer"
                style={{ marginLeft: 12 }}
              >
                Release notes ↗
              </a>
            )}
          </div>
        </div>
      )}

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

      {actionMsg && <div className="dim">{actionMsg}</div>}

      <h2>Service bindings</h2>
      <div className="panel">
        <table>
          <tbody>
            <tr>
              <td>Game distribution</td>
              <td>
                <code>{status.binds.game_distribution}</code>
              </td>
              <td style={{ textAlign: "right" }}>
                <button
                  style={{ padding: "4px 10px" }}
                  onClick={() => restartService("game", "Game distribution")}
                >
                  ↻ Restart
                </button>
              </td>
            </tr>
            <tr>
              <td>Shared pool</td>
              <td>
                <code>{status.binds.shared_pool}</code>
              </td>
              <td style={{ textAlign: "right" }}>
                <button
                  style={{ padding: "4px 10px" }}
                  onClick={() => restartService("share", "Shared pool")}
                >
                  ↻ Restart
                </button>
              </td>
            </tr>
            <tr>
              <td>Admin panel</td>
              <td>
                <code>{status.binds.admin_panel}</code>
              </td>
              <td style={{ textAlign: "right" }}>
                <button
                  style={{ padding: "4px 10px" }}
                  onClick={() => restartService("admin", "Admin panel")}
                >
                  ↻ Restart
                </button>
              </td>
            </tr>
          </tbody>
        </table>
        <p className="dim" style={{ marginBottom: 0 }}>
          A service restart is graceful: in-flight requests finish, then it
          rebinds on the same address. Use it to recover a wedged listener.
        </p>
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

      <h2>Server control</h2>
      <div className="panel">
        <button className="danger" onClick={restartServer}>
          ↻ Restart server process
        </button>
        <p className="dim" style={{ marginBottom: 0 }}>
          Bounces the whole server (all three services). Connected clients drop
          briefly and reconnect; downloads resume. Settings, library, and
          jukebox state are preserved.
        </p>
      </div>
    </>
  );
}
