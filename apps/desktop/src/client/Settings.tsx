import { FormEvent, useCallback, useEffect, useState } from "react";
import {
  api,
  AppBootState,
  ConnectionState,
  formatBytes,
  on,
  ServerRow,
  UpdateInfo,
} from "../lib/api";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

/** Manual self-update (F0.4): check on demand; "Download & restart" only ever
 * runs from the explicit button below — nothing is automatic. */
function UpdatesPanel({ version }: { version: string }) {
  const [checking, setChecking] = useState(false);
  const [installing, setInstalling] = useState(false);
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  const [msg, setMsg] = useState<string | null>(null);

  async function check() {
    setChecking(true);
    setMsg(null);
    try {
      const u = await api.updateCheck();
      setUpdate(u);
      if (!u) setMsg("You're on the latest version.");
    } catch (e) {
      // Offline → graceful no-op with a quiet note (F0.4).
      setMsg(`Couldn't reach the update server (offline?): ${String(e)}`);
    } finally {
      setChecking(false);
    }
  }

  async function install() {
    if (
      !window.confirm(
        `Download and install v${update?.version}? The app restarts when it finishes.`,
      )
    )
      return;
    setInstalling(true);
    try {
      await api.updateInstall(); // restarts on success
    } catch (e) {
      setMsg(String(e));
      setInstalling(false);
    }
  }

  return (
    <>
      <h2>Updates</h2>
      <div className="panel">
        <div className="row wrap">
          <span className="grow">
            Current version: <strong>v{version}</strong>
            {update && (
              <span className="badge blue" style={{ marginLeft: 10 }}>
                ⬆ v{update.version} available
              </span>
            )}
          </span>
          <button onClick={check} disabled={checking || installing}>
            {checking ? "Checking…" : "Check for updates"}
          </button>
          {update && (
            <button className="primary" onClick={install} disabled={installing}>
              {installing ? "Installing…" : "Download & restart"}
            </button>
          )}
        </div>
        {update?.notes && <pre style={{ marginTop: 10 }}>{update.notes}</pre>}
        {msg && <div className="dim" style={{ marginTop: 8 }}>{msg}</div>}
        <p className="dim" style={{ marginBottom: 0 }}>
          Updates are always manual — nothing downloads or restarts on its own.
        </p>
      </div>
    </>
  );
}

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

  // The row matching the server we're actually connected to (by game endpoint).
  const connectedHere = (s: ServerRow) =>
    conn.ws_connected && !!conn.game_endpoint && s.game_endpoint === conn.game_endpoint;

  return (
    <>
      <h1>Settings</h1>

      <h2>Connection</h2>
      <div className="panel">
        <div style={{ marginBottom: 4 }}>
          <span className={`conn-dot ${conn.ws_connected ? "on" : "off"}`} />
          {conn.ws_connected ? (
            <>
              Connected to{" "}
              <strong>{conn.server_label ?? conn.game_endpoint}</strong>
              {conn.game_endpoint ? ` (${conn.game_endpoint})` : ""}
            </>
          ) : conn.game_endpoint ? (
            `Reconnecting to ${conn.game_endpoint}…`
          ) : (
            "Not connected."
          )}
        </div>
        <div className="dim" style={{ marginBottom: 10, fontSize: 12 }}>
          Reconnects to your last server on launch and discovers others on the
          LAN automatically — no need to connect manually unless you switch
          servers.
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
                <tr
                  key={s.id}
                  style={
                    connectedHere(s)
                      ? { background: "rgba(52, 211, 153, 0.08)" }
                      : undefined
                  }
                >
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
                    {connectedHere(s) ? (
                      <span className="badge ok">✓ Connected</span>
                    ) : (
                      s.game_endpoint && (
                        <button
                          onClick={() =>
                            connect(s.game_endpoint!, s.share_endpoint, s.label)
                          }
                        >
                          Connect
                        </button>
                      )
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

      <UpdatesPanel version={boot.version} />

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
