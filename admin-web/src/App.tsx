import { useCallback, useEffect, useState } from "react";
import { api, AuthState, setUnauthorizedHandler } from "./api";
import Login from "./pages/Login";
import Dashboard from "./pages/Dashboard";
import Titles from "./pages/Titles";
import Shares from "./pages/Shares";
import Jukebox from "./pages/Jukebox";
import Settings from "./pages/Settings";
import LogView from "./pages/Log";

type Page = "dashboard" | "titles" | "shares" | "jukebox" | "settings" | "log";

const PAGES: { key: Page; label: string }[] = [
  { key: "dashboard", label: "Dashboard" },
  { key: "titles", label: "Game Library" },
  { key: "shares", label: "Shared Pool" },
  { key: "jukebox", label: "Jukebox" },
  { key: "settings", label: "Settings" },
  { key: "log", label: "Server Log" },
];

export default function App() {
  const [auth, setAuth] = useState<AuthState | null>(null);
  const [page, setPage] = useState<Page>("dashboard");

  const refreshAuth = useCallback(async () => {
    try {
      setAuth(await api.get<AuthState>("/api/auth-state"));
    } catch {
      setAuth({ needs_setup: false, authed: false });
    }
  }, []);

  useEffect(() => {
    setUnauthorizedHandler(() => setAuth((a) => (a ? { ...a, authed: false } : a)));
    void refreshAuth();
  }, [refreshAuth]);

  if (auth === null) {
    return <div className="login-wrap dim">Connecting to server…</div>;
  }
  if (!auth.authed) {
    return <Login needsSetup={auth.needs_setup} onAuthed={refreshAuth} />;
  }

  return (
    <div className="shell">
      <aside className="sidebar">
        <div className="brand">
          <span className="blt">BLT</span> Admin
          <small>Buttz LAN Tool</small>
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
        <nav className="nav">
          <button
            onClick={async () => {
              await api.post("/api/logout");
              void refreshAuth();
            }}
          >
            Log out
          </button>
        </nav>
      </aside>
      <main className="main">
        {page === "dashboard" && <Dashboard />}
        {page === "titles" && <Titles />}
        {page === "shares" && <Shares />}
        {page === "jukebox" && <Jukebox />}
        {page === "settings" && <Settings />}
        {page === "log" && <LogView />}
      </main>
    </div>
  );
}
