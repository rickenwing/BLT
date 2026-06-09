import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../api";

type Level = "all" | "info" | "warn" | "error";

function lineLevel(line: string): "trace" | "debug" | "info" | "warn" | "error" {
  if (line.includes(" ERROR ")) return "error";
  if (line.includes(" WARN ")) return "warn";
  if (line.includes(" DEBUG ")) return "debug";
  if (line.includes(" TRACE ")) return "trace";
  return "info";
}

export default function LogView() {
  const [lines, setLines] = useState<string[]>([]);
  const [level, setLevel] = useState<Level>("all");
  const [follow, setFollow] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const viewRef = useRef<HTMLDivElement>(null);

  const load = useCallback(async () => {
    try {
      const res = await api.get<{ lines: string[] }>("/api/log?lines=1000");
      setLines(res.lines);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : "failed");
    }
  }, []);

  useEffect(() => {
    void load();
    const t = setInterval(load, 3000);
    return () => clearInterval(t);
  }, [load]);

  useEffect(() => {
    if (follow && viewRef.current) {
      viewRef.current.scrollTop = viewRef.current.scrollHeight;
    }
  }, [lines, follow]);

  const visible = lines.filter((l) => {
    if (level === "all") return true;
    const ll = lineLevel(l);
    if (level === "error") return ll === "error";
    if (level === "warn") return ll === "warn" || ll === "error";
    return ll !== "debug" && ll !== "trace";
  });

  return (
    <>
      <div className="row">
        <h1 className="grow">Server Log</h1>
        <select
          value={level}
          onChange={(e) => setLevel(e.target.value as Level)}
          style={{ width: 130 }}
        >
          <option value="all">All levels</option>
          <option value="info">Info+</option>
          <option value="warn">Warn+</option>
          <option value="error">Errors</option>
        </select>
        <label className="row" style={{ gap: 6 }}>
          <input
            type="checkbox"
            checked={follow}
            onChange={(e) => setFollow(e.target.checked)}
            style={{ width: "auto" }}
          />
          <span className="dim">Follow</span>
        </label>
      </div>
      {error && <div className="error-text">{error}</div>}
      <div className="log-view" ref={viewRef}>
        {visible.length === 0 ? (
          <span className="dim">No log lines (yet).</span>
        ) : (
          visible.map((l, i) => (
            <div key={i} className={`log-line ${lineLevel(l)}`}>
              {l}
            </div>
          ))
        )}
      </div>
    </>
  );
}
