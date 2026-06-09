import { FormEvent, useCallback, useEffect, useState } from "react";
import { api, AppBootState, ConnectionState, formatBytes, on, ServerRow } from "../lib/api";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

export default function Settings({
  boot,
  conn,
}: {
  boot: AppBootState;
  conn: ConnectionState;
}) {
  const [name, setName] = useState(boot.settings.display_name);
  const [root, setRoot] = useState(boot.settings.default_download_root ?? "");
  const [cap, setCap] = useState(
    Math.round(boot.settings.upload_cap_bytes_per_sec / 1024),
  );
  const [shareBack, setShareBack] = useState(boot.settings.share_back);
  const [msg, setMsg] = useState<string | null>(null);
  const [servers, setServers] = useState<ServerRow[]>([]);
  const [manualGame, setManualGame] = useState("");
  const [manualShare, setManualShare] = useState("");
  const [lockPw, setLockPw] = useState("");

  const loadServers = useCallback(async () => {
    setServers(await api.listServers());
  }, []);

  useEffect(() => {
    void loadServers();
    const un = on("servers-changed", loadServers);
    return () => {
      void un.then((u) => u());
    };
  }, [loadServers]);

  async function save(e: FormEvent) {
    e.preventDefault();
    setMsg(null);
    try {
      await api.updateSettings({
        display_name: name,
        default_download_root: root,
        upload_cap_bytes_per_sec: cap * 1024,
        share_back: shareBack,
      });
      setMsg("Saved.");
    } catch (e2) {
      setMsg(String(e2));
    }
  }

  async function pickRoot() {
    const dir = await openDialog({ directory: true, title: "Default download folder" });
    if (typeof dir === "string") setRoot(dir);
  }

  async function connect(game: string, share?: string | null, label?: string | null) {
    try {
      await api.connectTo(game, share ?? null, label ?? null);
    } catch (e) {
      alert(String(e));
    }
  }

  async function enterLockdown() {
    if (!lockPw) return;
    if (
      !window.confirm(
        "Lock this machine to playback-only? It will restart into the jukebox UI " +
          "and can only be unlocked with the admin password.",
      )
    )
      return;
    try {
      await api.lockdownEnter(lockPw);
    } catch (e) {
      alert(String(e));
    }
  }

  return (
    <>
      <h1>Settings</h1>

      <h2>Connection</h2>
      <div className="panel">
        <div className="dim" style={{ marginBottom: 10 }}>
          {conn.ws_connected
            ? `Connected to ${conn.server_label ?? conn.game_endpoint}`
            : conn.game_endpoint
              ? `Reconnecting to ${conn.game_endpoint}…`
              : "Not connected."}
        </div>
        {servers.length > 0 && (
          <table style={{ marginBottom: 12 }}>
            <thead>
              <tr>
                <th>Server</th>
                <th>Game service</th>
                <th>Share service</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {servers.map((s) => (
                <tr key={s.id}>
                  <td>
                    <strong>{s.label ?? "(unnamed)"}</strong>
                    {s.uuid ? (
                      <span className="badge ok" style={{ marginLeft: 8 }}>
                        discovered
                      </span>
                    ) : (
                      <span className="badge" style={{ marginLeft: 8 }}>
                        manual
                      </span>
                    )}
                  </td>
                  <td className="dim">{s.game_endpoint}</td>
                  <td className="dim">{s.share_endpoint || "—"}</td>
                  <td>
                    {s.game_endpoint && (
                      <button
                        onClick={() => connect(s.game_endpoint!, s.share_endpoint, s.label)}
                      >
                        Connect
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <div className="row wrap">
          <input
            className="grow"
            placeholder="Manual: game service ip:port (e.g. 192.168.1.10:7400)"
            value={manualGame}
            onChange={(e) => setManualGame(e.target.value)}
          />
          <input
            className="grow"
            placeholder="share service ip:port (optional)"
            value={manualShare}
            onChange={(e) => setManualShare(e.target.value)}
          />
          <button
            onClick={() => manualGame && connect(manualGame, manualShare || null, null)}
          >
            Connect
          </button>
        </div>
      </div>

      <form onSubmit={save}>
        <h2>Identity</h2>
        <div className="panel">
          <label className="field">
            <span>Display name (shown on shares, votes, and the roster)</span>
            <input value={name} onChange={(e) => setName(e.target.value)} />
          </label>
        </div>

        <h2>Downloads</h2>
        <div className="panel">
          <label className="field">
            <span>Default download folder (per-title override at download time)</span>
            <div className="row">
              <input
                className="grow"
                value={root}
                onChange={(e) => setRoot(e.target.value)}
                placeholder="/Games"
              />
              <button type="button" onClick={pickRoot}>
                Browse…
              </button>
            </div>
          </label>
        </div>

        <h2>Sharing back (P2P)</h2>
        <div className="panel">
          <label className="row" style={{ marginBottom: 10 }}>
            <input
              type="checkbox"
              style={{ width: "auto" }}
              checked={shareBack}
              onChange={(e) => setShareBack(e.target.checked)}
            />
            <span>
              Seed downloaded games to other players (recommended — lightens the
              server's load)
            </span>
          </label>
          <label className="field">
            <span>
              Upload cap while seeding: {formatBytes(cap * 1024)}/s
            </span>
            <input
              type="range"
              min={256}
              max={10240}
              step={256}
              value={cap}
              onChange={(e) => setCap(Number(e.target.value))}
            />
          </label>
        </div>

        {msg && <div className="success-text">{msg}</div>}
        <button className="primary">Save settings</button>
      </form>

      <h2>Playback lockdown</h2>
      <div className="panel">
        <p className="dim">
          Turn this machine into the dedicated playback box (TV/projector). It
          restarts into a playback-only UI; games and shares are hidden.
          Entering and exiting requires the <strong>admin password</strong>.
        </p>
        <div className="row">
          <input
            type="password"
            placeholder="Admin password"
            value={lockPw}
            onChange={(e) => setLockPw(e.target.value)}
            style={{ width: 240 }}
          />
          <button onClick={enterLockdown}>Lock to playback</button>
        </div>
      </div>
    </>
  );
}
