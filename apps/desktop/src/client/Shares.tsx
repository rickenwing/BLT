import { useCallback, useEffect, useState } from "react";
import { api, confirmDialog, formatBytes, notify, ShareDownloadOut, ShareSummary } from "../lib/api";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";

export default function Shares() {
  const [shares, setShares] = useState<ShareSummary[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [dragOver, setDragOver] = useState(false);
  const [busy, setBusy] = useState(false);
  const [lastResult, setLastResult] = useState<Record<number, ShareDownloadOut>>({});
  const [myClientId, setMyClientId] = useState("");

  const load = useCallback(async () => {
    try {
      setShares(await api.sharesList());
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void load();
    void api.getAppState().then((b) => setMyClientId(b.settings.client_id));
    const t = setInterval(load, 8000);
    // Native OS file drop (F6.3) — confirm before any bytes move (#5).
    const un = getCurrentWebview().onDragDropEvent((event) => {
      if (event.payload.type === "over") setDragOver(true);
      else if (event.payload.type === "leave") setDragOver(false);
      else if (event.payload.type === "drop") {
        setDragOver(false);
        void confirmAndUpload(event.payload.paths);
      }
    });
    return () => {
      clearInterval(t);
      void un.then((u) => u());
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function confirmAndUpload(paths: string[]) {
    if (paths.length === 0) return;
    const names = paths.map((p) => p.split(/[\\/]/).pop()).join(", ");
    if (!await confirmDialog(`Share with the group?\n\n${names}\n\nFolders upload their whole tree.`)) {
      return;
    }
    setBusy(true);
    try {
      await api.shareUpload(paths);
      await load();
    } catch (e) {
      void notify(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function pickAndUpload(directory: boolean) {
    const picked = await openDialog({
      directory,
      multiple: !directory,
      title: directory ? "Share a folder" : "Share file(s)",
    });
    if (!picked) return;
    const paths = Array.isArray(picked) ? picked : [picked];
    await confirmAndUpload(paths);
  }

  async function download(s: ShareSummary, onlyMissing: boolean) {
    const dir = await openDialog({ directory: true, title: "Download share to…" });
    if (typeof dir !== "string") return;
    // Free-space pre-flight (F6.6): warn + confirm, never block (#6).
    const [enough, avail] = await api.preflightDest(dir, s.size);
    if (!enough) {
      const cont = await confirmDialog(
        `The destination may not have enough space (${formatBytes(avail ?? 0)} free, ` +
          `${formatBytes(s.size)} needed). Continue anyway?`,
      );
      if (!cont) return;
    }
    if (!await confirmDialog(`Download "${s.name}" (${formatBytes(s.size)}) to ${dir}?`)) return;
    setBusy(true);
    try {
      const res = await api.shareDownload(s.id, dir, onlyMissing);
      setLastResult((m) => ({ ...m, [s.id]: res }));
    } catch (e) {
      void notify(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function remove(s: ShareSummary) {
    if (!await confirmDialog(`Delete your share "${s.name}"? This removes it for everyone.`)) return;
    try {
      await api.shareDelete(s.id);
      await load();
    } catch (e) {
      void notify(e instanceof Error ? e.message : String(e));
    }
  }

  return (
    <>
      <div className="row">
        <h1 className="grow">Shared Pool</h1>
        <button onClick={() => pickAndUpload(false)} disabled={busy}>
          ⬆ Share files…
        </button>
        <button onClick={() => pickAndUpload(true)} disabled={busy}>
          ⬆ Share a folder…
        </button>
      </div>
      {error && <div className="error-text">{error}</div>}

      <div className={`dropzone ${dragOver ? "over" : ""}`}>
        Drop files or folders here to share them with the group
      </div>

      <div className="panel">
        {shares.length === 0 ? (
          <span className="dim">Nothing here yet — drop something in!</span>
        ) : (
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th>Kind</th>
                <th>Size</th>
                <th>Files</th>
                <th>From</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {shares.map((s) => (
                <tr key={s.id}>
                  <td>
                    <strong>{s.name}</strong>
                    {lastResult[s.id] && (
                      <div
                        className={
                          lastResult[s.id].present === lastResult[s.id].total
                            ? "success-text"
                            : "error-text"
                        }
                        style={{ fontSize: 12 }}
                      >
                        {lastResult[s.id].present} of {lastResult[s.id].total} files
                        {lastResult[s.id].present < lastResult[s.id].total && " — incomplete"}
                      </div>
                    )}
                  </td>
                  <td>
                    <span className="badge">{s.kind}</span>
                  </td>
                  <td>{formatBytes(s.size)}</td>
                  <td>{s.file_count}</td>
                  <td>{s.owner_name}</td>
                  <td>
                    <div className="row">
                      <button disabled={busy} onClick={() => download(s, false)}>
                        ⬇ Download
                      </button>
                      {lastResult[s.id] && lastResult[s.id].present < lastResult[s.id].total && (
                        <button disabled={busy} onClick={() => download(s, true)}>
                          Fetch missing
                        </button>
                      )}
                      {/* uploader-only delete (F6.9); admin deletes via panel */}
                      {myClientId && (
                        <button className="danger" disabled={busy} onClick={() => remove(s)}>
                          Delete
                        </button>
                      )}
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
      <p className="dim">
        Shares are stored on the server and survive between parties. Deleting is
        limited to whoever uploaded a share (the admin can moderate from the
        panel).
      </p>
    </>
  );
}
