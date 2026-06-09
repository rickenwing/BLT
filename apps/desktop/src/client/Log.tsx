import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../lib/api";

type Level = "all" | "info" | "warn" | "error";

function lineLevel(line: string): "info" | "warn" | "error" | "debug" {
  if (line.includes(" ERROR ")) return "error";
  if (line.includes(" WARN ")) return "warn";
  if (line.includes(" DEBUG ") || line.includes(" TRACE ")) return "debug";
  return "info";
}

export default function LogView() {
  const [lines, setLines] = useState<string[]>([]);
  const [level, setLevel] = useState<Level>("all");
  const viewRef = useRef<HTMLDivElement>(null);

  const load = useCallback(async () => {
    try {
      setLines(await api.logTail(800));
    } catch {
      /* log dir may not exist yet */
    }
  }, []);

  useEffect(() => {
    void load();
    const t = setInterval(load, 3000);
    return () => clearInterval(t);
  }, [load]);

  useEffect(() => {
    if (viewRef.current) viewRef.current.scrollTop = viewRef.current.scrollHeight;
  }, [lines]);

  const visible = lines.filter((l) => {
    const ll = lineLevel(l);
    if (level === "all") return true;
    if (level === "error") return ll === "error";
    if (level === "warn") return ll === "warn" || ll === "error";
    return ll !== "debug";
  });

  return (
    <>
      <div className="row">
        <h1 className="grow">Client Log</h1>
        <select
          value={level}
          onChange={(e) => setLevel(e.target.value as Level)}
          style={{ width: 120 }}
        >
          <option value="all">All</option>
          <option value="info">Info+</option>
          <option value="warn">Warn+</option>
          <option value="error">Errors</option>
        </select>
      </div>
      <div className="log-view" ref={viewRef}>
        {visible.length === 0 ? (
          <span className="dim">No log lines yet.</span>
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
