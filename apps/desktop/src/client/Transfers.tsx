import { useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api, formatBytes, formatSpeed, on, TransferRow } from "../lib/api";

const APP_TITLE = "Buttz LAN Tool";

const arrow = (kind: TransferRow["kind"]) => (kind === "share-upload" ? "↑" : "↓");
const pctOf = (t: TransferRow) =>
  t.total ? Math.min(100, Math.floor((100 * t.done) / t.total)) : 0;

/**
 * Uniform transfer indicator (sidebar + window title) covering game downloads
 * and shared-pool uploads/downloads. Polls every 1s and refreshes on the
 * transfer/download events.
 */
export default function Transfers() {
  const [items, setItems] = useState<TransferRow[]>([]);

  useEffect(() => {
    let alive = true;
    const load = async () => {
      const t = await api.activeTransfers();
      if (alive) setItems(t);
    };
    void load();
    const poll = setInterval(load, 1000);
    const subs = [on("transfers-changed", load), on("downloads-changed", load)];
    return () => {
      alive = false;
      clearInterval(poll);
      subs.forEach((s) => void s.then((u) => u()));
    };
  }, []);

  // Mirror transfer status into the OS title bar.
  useEffect(() => {
    const w = getCurrentWindow();
    if (items.length === 0) {
      void w.setTitle(APP_TITLE);
    } else if (items.length === 1) {
      const t = items[0];
      const spd = t.speed_bps ? ` · ${formatSpeed(t.speed_bps)}` : "";
      void w.setTitle(`${arrow(t.kind)} ${pctOf(t)}%${spd} — ${APP_TITLE}`);
    } else {
      void w.setTitle(`⇅ ${items.length} transfers — ${APP_TITLE}`);
    }
  }, [items]);

  if (items.length === 0) return null;

  return (
    <div className="transfers">
      <div className="transfers-head">Transfers</div>
      {items.map((t) => {
        const pct = pctOf(t);
        return (
          <div className="xfer" key={t.id}>
            <div className="xfer-row">
              <span className="xfer-name" title={t.label}>
                {arrow(t.kind)} {t.label}
              </span>
              <span className="xfer-pct">{pct}%</span>
            </div>
            <div className="progress sm">
              <div style={{ width: `${pct}%` }} />
            </div>
            <div className="xfer-sub">
              {formatBytes(t.done)} / {formatBytes(t.total)}
              {t.speed_bps ? ` · ${formatSpeed(t.speed_bps)}` : ""}
            </div>
          </div>
        );
      })}
    </div>
  );
}
