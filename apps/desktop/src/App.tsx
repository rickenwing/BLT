import { useCallback, useEffect, useState } from "react";
import { api, AppBootState, on } from "./lib/api";
import ClientShell from "./client/ClientShell";
import Playback from "./playback/Playback";

export default function App() {
  const [boot, setBoot] = useState<AppBootState | null>(null);

  const refresh = useCallback(async () => {
    setBoot(await api.getAppState());
  }, []);

  useEffect(() => {
    void refresh();
    const un = on("connection-changed", refresh);
    return () => {
      void un.then((u) => u());
    };
  }, [refresh]);

  if (!boot) return <div className="firstrun dim">Starting…</div>;

  // First run: pick client vs playback (F0.5).
  if (!boot.mode_chosen) {
    return (
      <div className="firstrun">
        <div className="mode-card" onClick={() => api.chooseMode("client")}>
          <div className="icon">🎮</div>
          <h2 style={{ color: "var(--text)" }}>Player</h2>
          <p className="dim">
            Browse and download games, share files, queue videos. The normal
            choice for your gaming machine.
          </p>
        </div>
        <div className="mode-card" onClick={() => api.chooseMode("playback")}>
          <div className="icon">📺</div>
          <h2 style={{ color: "var(--text)" }}>Playback machine</h2>
          <p className="dim">
            The box wired to the TV/projector. Plays the video jukebox for the
            room; never downloads games.
          </p>
        </div>
      </div>
    );
  }

  if (boot.settings.mode === "playback") {
    return <Playback boot={boot} />;
  }
  return <ClientShell boot={boot} />;
}
