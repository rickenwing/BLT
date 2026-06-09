import { useEffect, useState } from "react";
import { api, AppBootState, ConnectionState, on } from "../lib/api";
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

  useEffect(() => {
    const un = on("connection-changed", async () => {
      setConn(await api.connectionState());
    });
    return () => {
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
