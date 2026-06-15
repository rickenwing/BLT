import { useEffect, useState } from "react";
import { api, AppBootState, ConnectionState, on, UpdateInfo } from "../lib/api";
import Library from "./Library";
import Downloads from "./Downloads";
import Shares from "./Shares";
import Jukebox from "./Jukebox";
import People from "./People";
import Settings from "./Settings";
import LogView from "./Log";

type Page = "library" | "downloads" | "shares" | "jukebox" | "people" | "settings" | "log";

const PAGES: { key: Page; label: string }[] = [
  { key: "library", label: "Game Library" },
  { key: "downloads", label: "Downloads" },
  { key: "shares", label: "Shared Pool" },
  { key: "jukebox", label: "Jukebox" },
  { key: "people", label: "People" },
  { key: "settings", label: "Settings" },
  { key: "log", label: "Log" },
];

export default function ClientShell({ boot }: { boot: AppBootState }) {
  const [page, setPage] = useState<Page>("library");
  const [conn, setConn] = useState<ConnectionState>(boot.connection);
  const [update, setUpdate] = useState<UpdateInfo | null>(null);

  // Launch-time check (F0.4): indicator only — never downloads or installs.
  // Offline / unreachable is a silent no-op.
  useEffect(() => {
    void api.updateCheck().then(setUpdate).catch(() => undefined);
  }, []);

  useEffect(() => {
    let alive = true;
    const refresh = async () => {
      const c = await api.connectionState();
      if (alive) setConn(c);
    };
    const un = on("connection-changed", refresh);
    // Reconcile right after subscribing: the WS often connects (and emits
    // "connection-changed") during app boot, *before* this listener is
    // registered — Tauri doesn't buffer events for late subscribers, and a
    // stable connection never emits again. Without this the indicator sticks on
    // "reconnecting…" forever despite being connected.
    void refresh();
    // Safety net so the status can never get wedged on a single missed event.
    const t = setInterval(refresh, 4000);
    return () => {
      alive = false;
      clearInterval(t);
      void un.then((u) => u());
    };
  }, []);

  return (
    <div className="shell">
      <aside className="sidebar">
        <div className="brand">
          <span className="blt">BLT</span> Buttz LAN Tool
          <small>
            <span className={`conn-dot ${conn.ws_connected ? "on" : "off"}`} />
            {conn.ws_connected
              ? (conn.server_label ?? conn.game_endpoint ?? "connected")
              : conn.game_endpoint
                ? "reconnecting…"
                : "not connected"}
          </small>
        </div>
        <nav className="nav">
          {PAGES.map((p) => (
            <button
              key={p.key}
              className={page === p.key ? "active" : ""}
              onClick={() => setPage(p.key)}
            >
              {p.label}
            </button>
          ))}
        </nav>
        <div className="spacer" />
        {update && (
          <nav className="nav">
            <button onClick={() => setPage("settings")}>
              <span className="badge blue">⬆ v{update.version} available</span>
            </button>
          </nav>
        )}
        <div className="foot">
          {boot.settings.display_name} · v{boot.version}
        </div>
      </aside>
      <main className="main">
        {page === "library" && <Library connected={conn.ws_connected} />}
        {page === "downloads" && <Downloads />}
        {page === "shares" && <Shares />}
        {page === "jukebox" && <Jukebox />}
        {page === "people" && <People />}
        {page === "settings" && <Settings boot={boot} conn={conn} />}
        {page === "log" && <LogView />}
      </main>
    </div>
  );
}
