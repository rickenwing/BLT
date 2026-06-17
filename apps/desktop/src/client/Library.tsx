import { useCallback, useEffect, useState } from "react";
import {
  api,
  confirmDialog,
  formatBytes,
  notify,
  on,
  ScriptPreview,
  Title,
  TitleInfo,
  ValidationOut,
} from "../lib/api";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

const STATE_BADGE: Record<Title["local_state"], { label: string; cls: string }> = {
  not_downloaded: { label: "", cls: "" },
  partial: { label: "partial", cls: "warn" },
  complete: { label: "installed", cls: "ok" },
  update_available: { label: "update available", cls: "blue" },
};

export default function Library({ connected }: { connected: boolean }) {
  const [titles, setTitles] = useState<Title[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Title | null>(null);
  const [covers, setCovers] = useState<Record<number, string>>({});
  const [infos, setInfos] = useState<Record<number, TitleInfo>>({});

  const load = useCallback(async () => {
    try {
      const ts = await api.fetchTitles();
      setTitles(ts);
      setError(null);
      // Lazily pull info payloads (cached by info_hash on the Rust side, F4.1).
      for (const t of ts) {
        if (t.info_hash) {
          void api
            .fetchTitleInfo(t.id, t.info_hash)
            .then((info) => {
              setInfos((m) => ({ ...m, [t.id]: info }));
              if (info.cover_b64) {
                setCovers((m) => ({ ...m, [t.id]: info.cover_b64! }));
              }
            })
            .catch(() => undefined);
        }
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, []);

  useEffect(() => {
    void load();
    const un1 = on("titles-changed", load);
    const un2 = on("download-complete", load);
    const un3 = on("connection-changed", load);
    return () => {
      [un1, un2, un3].forEach((u) => void u.then((f) => f()));
    };
  }, [load]);

  if (!connected && titles.length === 0) {
    return (
      <>
        <h1>Game Library</h1>
        <div className="panel dim">
          Not connected — pick a server under Settings → Connection. Titles
          appear here once connected.
        </div>
        {error && <div className="error-text">{error}</div>}
      </>
    );
  }

  return (
    <>
      <div className="row">
        <h1 className="grow">Game Library</h1>
        <DownloadAll titles={titles} reload={load} />
      </div>
      {error && <div className="error-text">{error}</div>}
      {titles.length === 0 ? (
        <div className="panel dim">No titles published yet.</div>
      ) : (
        <div className="grid">
          {titles.map((t) => (
            <div
              key={t.id}
              className={`tile ${selected?.id === t.id ? "selected" : ""}`}
              onClick={() => setSelected(t)}
            >
              {covers[t.id] ? (
                <img
                  className="cover"
                  src={`data:image;base64,${covers[t.id]}`}
                  alt=""
                />
              ) : (
                <div className="cover placeholder">
                  {(t.label || t.name).slice(0, 1).toUpperCase()}
                </div>
              )}
              <div className="meta">
                <div className="t">{t.label || infos[t.id]?.name || t.name}</div>
                <div className="s">
                  <span>{formatBytes(t.total_size)}</span>
                  {STATE_BADGE[t.local_state].label && (
                    <span className={`badge ${STATE_BADGE[t.local_state].cls}`}>
                      {STATE_BADGE[t.local_state].label}
                    </span>
                  )}
                </div>
              </div>
            </div>
          ))}
        </div>
      )}
      {selected && (
        <TitleDetail
          title={titles.find((t) => t.id === selected.id) ?? selected}
          info={infos[selected.id]}
          cover={covers[selected.id]}
          onClose={() => setSelected(null)}
          reload={load}
        />
      )}
    </>
  );
}

/** "Download all" with its explicit confirmation gate (#5 / F4.2). */
function DownloadAll({ titles, reload }: { titles: Title[]; reload: () => void }) {
  const [busy, setBusy] = useState(false);
  const pending = titles.filter((t) => t.local_state === "not_downloaded");
  if (pending.length === 0) return null;
  const total = pending.reduce((s, t) => s + t.total_size, 0);

  async function run() {
    if (
      !await confirmDialog(
        `Download ALL ${pending.length} titles (${formatBytes(total)} total)? They queue sequentially.`,
      )
    )
      return;
    setBusy(true);
    try {
      for (const t of pending) {
        const plan = await api.prepareDownload(t.id, t.name, t.total_size, null);
        if (!plan.enough_space) {
          const cont = await confirmDialog(
            `${t.label || t.name}: destination may not have enough space ` +
              `(${formatBytes(plan.available_bytes ?? 0)} free, ${formatBytes(t.total_size)} needed). Continue anyway?`,
          );
          if (!cont) continue;
        }
        await api.beginDownload(t.id, t.manifest_ver, t.label || t.name, plan.dest);
      }
      reload();
    } catch (e) {
      void notify(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }
  return (
    <button onClick={run} disabled={busy}>
      ⬇ Download all ({pending.length})
    </button>
  );
}

function TitleDetail({
  title,
  info,
  cover,
  onClose,
  reload,
}: {
  title: Title;
  info?: TitleInfo;
  cover?: string;
  onClose: () => void;
  reload: () => void;
}) {
  const [busy, setBusy] = useState<string | null>(null);
  const [validation, setValidation] = useState<ValidationOut | null>(null);
  const [script, setScript] = useState<ScriptPreview | null>(null);
  const [scriptOut, setScriptOut] = useState<string | null>(null);
  const name = title.label || info?.name || title.name;

  async function startDownload(destOverride?: string) {
    setBusy("download");
    try {
      const plan = await api.prepareDownload(
        title.id,
        title.name,
        title.total_size,
        destOverride ?? null,
      );
      // Free-space pre-flight: warn + confirm, never hard-block (F4.4 / #6).
      if (!plan.enough_space) {
        const cont = await confirmDialog(
          `The destination volume may not have enough space:\n\n` +
            `needed ${formatBytes(plan.needed_bytes)}, free ${formatBytes(plan.available_bytes ?? 0)}.\n\n` +
            `Download anyway (you can free space while it runs)?`,
        );
        if (!cont) return;
      }
      await api.beginDownload(title.id, title.manifest_ver, name, plan.dest);
      reload();
    } catch (e) {
      void notify(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }

  async function pickDestAndDownload() {
    const dir = await openDialog({ directory: true, title: "Install folder for this title" });
    if (typeof dir === "string") {
      await startDownload(`${dir}/${title.name}`);
    }
  }

  async function validate(deep: boolean) {
    setBusy(deep ? "deep" : "quick");
    setValidation(null);
    try {
      setValidation(await api.validateTitle(title.id, deep));
    } catch (e) {
      void notify(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }

  async function repair() {
    setBusy("repair");
    try {
      const n = await api.repairTitle(title.id, name);
      void notify(n === 0 ? "Nothing to repair — all chunks verify." : `Re-fetching ${n} chunks…`);
      reload();
    } catch (e) {
      void notify(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }

  async function showScript() {
    try {
      const s = await api.scriptPreview(title.id);
      if (!s) {
        void notify("No post-install script for this title.");
        return;
      }
      setScript(s);
    } catch (e) {
      void notify(e instanceof Error ? e.message : String(e));
    }
  }

  async function runScript() {
    if (!script) return;
    setBusy("script");
    try {
      const res = await api.scriptRun(title.id, script.hash);
      setScriptOut(
        `${res.success ? "✓ succeeded" : `✗ failed (exit ${res.exit_code ?? "?"})`}\n${res.output}`,
      );
    } catch (e) {
      setScriptOut(`✗ ${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setBusy(null);
    }
  }

  async function deleteGame() {
    if (
      !await confirmDialog(
        `Delete "${title.label || title.name}" and remove its files from disk?\n\n` +
          `${title.local_dest ?? ""}\n\nThis cannot be undone.`,
      )
    )
      return;
    setBusy("delete");
    try {
      await api.deleteGame(title.id);
      onClose();
    } catch (e) {
      void notify(String(e));
    } finally {
      setBusy(null);
    }
  }

  const downloadable =
    title.local_state === "not_downloaded" || title.local_state === "update_available";

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" style={{ width: 620 }} onClick={(e) => e.stopPropagation()}>
        <div className="row" style={{ alignItems: "flex-start" }}>
          {cover ? (
            <img className="detail-cover" src={`data:image;base64,${cover}`} alt="" />
          ) : (
            <div
              className="detail-cover cover placeholder tile"
              style={{ width: 180, aspectRatio: "3/4", display: "flex", alignItems: "center", justifyContent: "center", fontSize: 48 }}
            >
              {name.slice(0, 1).toUpperCase()}
            </div>
          )}
          <div className="grow">
            <h3 style={{ marginBottom: 4 }}>{name}</h3>
            <div className="dim" style={{ marginBottom: 8 }}>
              {[info?.year, info?.genre, info?.players].filter(Boolean).join(" · ") || " "}
            </div>
            {info?.blurb && <p style={{ marginTop: 0 }}>{info.blurb}</p>}
            <div className="row wrap" style={{ marginBottom: 8 }}>
              <span className="badge">{formatBytes(title.total_size)}</span>
              <span className="badge">{title.file_count} files</span>
              <span className="badge">v{title.manifest_ver}</span>
              {title.has_install_script && <span className="badge warn">post-install script</span>}
              {STATE_BADGE[title.local_state].label && (
                <span className={`badge ${STATE_BADGE[title.local_state].cls}`}>
                  {STATE_BADGE[title.local_state].label}
                </span>
              )}
            </div>
            {title.local_dest && (
              <div className="dim" style={{ fontSize: 12, marginBottom: 8 }}>
                📁 {title.local_dest}
              </div>
            )}

            <div className="row wrap">
              {downloadable && (
                <>
                  <button className="primary" disabled={busy !== null} onClick={() => startDownload()}>
                    {title.local_state === "update_available" ? "Download update" : "Download"}
                  </button>
                  <button disabled={busy !== null} onClick={pickDestAndDownload}>
                    Download to…
                  </button>
                </>
              )}
              {title.local_state === "complete" && (
                <>
                  {(info?.launch?.length ?? 0) > 0 &&
                    info!.launch!.map((l, i) => (
                      <button
                        key={i}
                        className={i === 0 ? "primary" : ""}
                        onClick={() => api.launchTitle(title.id, title.info_hash, i).catch((e) => void notify(String(e)))}
                      >
                        ▶ {l.name}
                      </button>
                    ))}
                  <button disabled={busy !== null} onClick={() => validate(false)}>
                    Quick validate
                  </button>
                  <button disabled={busy !== null} onClick={() => validate(true)}>
                    Deep verify
                  </button>
                  {title.has_install_script && (
                    <button disabled={busy !== null} onClick={showScript}>
                      Setup script…
                    </button>
                  )}
                </>
              )}
              {title.local_state !== "not_downloaded" && (
                <button className="danger" disabled={busy !== null} onClick={deleteGame}>
                  🗑 Delete game
                </button>
              )}
            </div>

            {validation && (
              <div className="panel" style={{ marginTop: 12 }}>
                {validation.all_ok ? (
                  <span className="success-text">
                    ✓ All {validation.total} files OK
                  </span>
                ) : (
                  <>
                    <span className="error-text">
                      {validation.ok_count}/{validation.total} files OK —{" "}
                      {validation.failures.length} failed
                    </span>
                    <ul className="dim" style={{ fontSize: 12 }}>
                      {validation.failures.slice(0, 8).map(([p, d]) => (
                        <li key={p}>
                          {p} — {d}
                        </li>
                      ))}
                    </ul>
                    <button disabled={busy !== null} onClick={repair}>
                      Repair (re-fetch bad chunks)
                    </button>
                  </>
                )}
              </div>
            )}
          </div>
        </div>

        {/* Post-install script: contents shown + explicit confirm (F16.4) */}
        {script && (
          <div className="panel" style={{ marginTop: 12 }}>
            <h3>Post-install script</h3>
            <p className="dim">
              This script came from the server and will run in the install
              folder. Review it before running — it executes with your user
              account.
            </p>
            <pre>{script.contents}</pre>
            {scriptOut ? (
              <pre>{scriptOut}</pre>
            ) : script.runnable_here ? (
              <div className="row">
                <button className="primary" disabled={busy !== null} onClick={runScript}>
                  I reviewed it — run the script
                </button>
                <button onClick={() => setScript(null)}>Cancel</button>
              </div>
            ) : (
              <span className="dim">Scripts only run on Windows clients.</span>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
