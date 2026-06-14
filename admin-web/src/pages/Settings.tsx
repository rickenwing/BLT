import { FormEvent, useEffect, useState } from "react";
import { api, InterfaceInfo, ServerConfig } from "../api";

/** Split "ip:port" into editable parts (best-effort). */
function splitBind(bind: string): { ip: string; port: string } {
  const i = bind.lastIndexOf(":");
  return i === -1
    ? { ip: bind, port: "" }
    : { ip: bind.slice(0, i), port: bind.slice(i + 1) };
}

function BindEditor({
  label,
  value,
  interfaces,
  onChange,
}: {
  label: string;
  value: string;
  interfaces: InterfaceInfo[];
  onChange: (v: string) => void;
}) {
  const { ip, port } = splitBind(value);
  const options = [
    { ip: "0.0.0.0", name: "All interfaces" },
    { ip: "127.0.0.1", name: "Localhost only" },
    ...interfaces
      .filter((i) => !i.is_loopback)
      .map((i) => ({ ip: i.ip, name: `${i.name} (${i.ip})` })),
  ];
  const known = options.some((o) => o.ip === ip);
  return (
    <label className="field">
      <span>{label}</span>
      <div className="row">
        <select
          className="grow"
          value={known ? ip : "custom"}
          onChange={(e) => {
            if (e.target.value !== "custom") onChange(`${e.target.value}:${port}`);
          }}
        >
          {options.map((o) => (
            <option key={o.ip} value={o.ip}>
              {o.name}
            </option>
          ))}
          {!known && <option value="custom">{ip} (custom)</option>}
        </select>
        <input
          style={{ width: 100 }}
          value={port}
          onChange={(e) => onChange(`${ip}:${e.target.value}`)}
          placeholder="port"
        />
      </div>
    </label>
  );
}

type PathField = "library_path" | "staging_path" | "share_path";

interface FsListing {
  path: string;
  parent: string | null;
  dirs: { name: string; path: string }[];
}

/** Server-side folder picker (ENH-1): browses the server's own filesystem. */
function FolderPicker({
  initial,
  onPick,
  onClose,
}: {
  initial: string;
  onPick: (path: string) => void;
  onClose: () => void;
}) {
  const [cur, setCur] = useState<FsListing | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const load = async (p: string) => {
    try {
      setErr(null);
      setCur(await api.get<FsListing>(`/api/fs?path=${encodeURIComponent(p)}`));
    } catch (e) {
      setErr(e instanceof Error ? e.message : "failed");
    }
  };
  useEffect(() => {
    void load(initial);
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3 style={{ marginTop: 0 }}>Choose a folder</h3>
        {err && <div className="error-text">{err}</div>}
        {cur && (
          <>
            <div className="dim" style={{ wordBreak: "break-all", marginBottom: 8 }}>
              {cur.path}
            </div>
            <div className="picker-list">
              {cur.parent && (
                <button className="picker-row" onClick={() => load(cur.parent!)}>
                  ⬆ ..
                </button>
              )}
              {cur.dirs.map((d) => (
                <button key={d.path} className="picker-row" onClick={() => load(d.path)}>
                  📁 {d.name}
                </button>
              ))}
              {cur.dirs.length === 0 && <div className="dim">No sub-folders.</div>}
            </div>
            <div className="row" style={{ justifyContent: "flex-end", gap: 8, marginTop: 12 }}>
              <button onClick={onClose}>Cancel</button>
              <button className="primary" onClick={() => onPick(cur.path)}>
                Select this folder
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

export default function Settings() {
  const [cfg, setCfg] = useState<ServerConfig | null>(null);
  const [pickerFor, setPickerFor] = useState<PathField | null>(null);
  const [interfaces, setInterfaces] = useState<InterfaceInfo[]>([]);
  const [msg, setMsg] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pwCurrent, setPwCurrent] = useState("");
  const [pwNew, setPwNew] = useState("");
  const [pwMsg, setPwMsg] = useState<string | null>(null);

  useEffect(() => {
    void (async () => {
      try {
        setCfg(await api.get<ServerConfig>("/api/config"));
        setInterfaces(await api.get<InterfaceInfo[]>("/api/interfaces"));
      } catch (e) {
        setError(e instanceof Error ? e.message : "failed");
      }
    })();
  }, []);

  if (error) return <div className="error-text">{error}</div>;
  if (!cfg) return <div className="dim">Loading…</div>;

  const set = (patch: Partial<ServerConfig>) => setCfg({ ...cfg, ...patch });

  async function save(e: FormEvent) {
    e.preventDefault();
    setMsg(null);
    setError(null);
    try {
      const res = await api.put<{ ok: boolean; rebinding: boolean }>(
        "/api/config",
        cfg,
      );
      setMsg(
        res.rebinding
          ? "Saved. Affected listeners are rebinding — if you changed the admin bind, reconnect on the new address."
          : "Saved.",
      );
    } catch (err) {
      setError(err instanceof Error ? err.message : "save failed");
    }
  }

  async function changePassword(e: FormEvent) {
    e.preventDefault();
    setPwMsg(null);
    try {
      await api.put("/api/password", { current: pwCurrent, new: pwNew });
      setPwMsg("Password changed.");
      setPwCurrent("");
      setPwNew("");
    } catch (err) {
      setPwMsg(err instanceof Error ? `Failed: ${err.message}` : "failed");
    }
  }

  return (
    <>
      <h1>Settings</h1>
      <form onSubmit={save}>
        <h2>Server</h2>
        <div className="panel">
          <label className="field">
            <span>Server label (shown to clients via discovery)</span>
            <input
              value={cfg.server_label}
              onChange={(e) => set({ server_label: e.target.value })}
            />
          </label>
        </div>

        <h2>Service bindings — each can use its own NIC</h2>
        <div className="panel">
          <BindEditor
            label="Game distribution"
            value={cfg.game_distribution_bind}
            interfaces={interfaces}
            onChange={(v) => set({ game_distribution_bind: v })}
          />
          <BindEditor
            label="Shared pool"
            value={cfg.shared_pool_bind}
            interfaces={interfaces}
            onChange={(v) => set({ shared_pool_bind: v })}
          />
          <BindEditor
            label="Admin panel"
            value={cfg.admin_panel_bind}
            interfaces={interfaces}
            onChange={(v) => set({ admin_panel_bind: v })}
          />
          <p className="dim">
            Locked out by a bad admin bind? Edit <code>config.toml</code> in the
            server data folder and restart, or run{" "}
            <code>blt-server --reset-admin-bind</code>.
          </p>
        </div>

        <h2>Storage paths</h2>
        <div className="panel">
          <label className="field">
            <span>Game library (each subfolder = one title, stored unzipped)</span>
            <div className="row">
              <input
                className="grow"
                value={cfg.library_path ?? ""}
                onChange={(e) => set({ library_path: e.target.value || null })}
                placeholder="/Volumes/Games/library"
              />
              <button type="button" onClick={() => setPickerFor("library_path")}>
                Browse…
              </button>
            </div>
          </label>
          <label className="field">
            <span>
              Staging (same volume as the library — promoted after the settle
              window)
            </span>
            <div className="row">
              <input
                className="grow"
                value={cfg.staging_path ?? ""}
                onChange={(e) => set({ staging_path: e.target.value || null })}
                placeholder="/Volumes/Games/staging"
              />
              <button type="button" onClick={() => setPickerFor("staging_path")}>
                Browse…
              </button>
            </div>
          </label>
          <label className="field">
            <span>Shared pool drive</span>
            <div className="row">
              <input
                className="grow"
                value={cfg.share_path ?? ""}
                onChange={(e) => set({ share_path: e.target.value || null })}
                placeholder="/Volumes/Share/pool"
              />
              <button type="button" onClick={() => setPickerFor("share_path")}>
                Browse…
              </button>
            </div>
          </label>
        </div>

        <h2>Tuning</h2>
        <div className="panel">
          <div className="row wrap">
            <label className="field grow">
              <span>Staging settle (seconds)</span>
              <input
                type="number"
                value={cfg.staging_settle_secs}
                onChange={(e) =>
                  set({ staging_settle_secs: Number(e.target.value) })
                }
              />
            </label>
            <label className="field grow">
              <span>Auto-scan interval (seconds)</span>
              <input
                type="number"
                value={cfg.scan_interval_secs}
                onChange={(e) =>
                  set({ scan_interval_secs: Number(e.target.value) })
                }
              />
            </label>
            <label className="field grow">
              <span>DB backup interval (seconds)</span>
              <input
                type="number"
                value={cfg.db_backup_interval_secs}
                onChange={(e) =>
                  set({ db_backup_interval_secs: Number(e.target.value) })
                }
              />
            </label>
            <label className="field grow">
              <span>Peer timeout (seconds)</span>
              <input
                type="number"
                value={cfg.peer_timeout_secs}
                onChange={(e) =>
                  set({ peer_timeout_secs: Number(e.target.value) })
                }
              />
            </label>
          </div>
        </div>

        {msg && <div className="success-text">{msg}</div>}
        {error && <div className="error-text">{error}</div>}
        <button className="primary">Save settings</button>
      </form>

      <h2>Admin password</h2>
      <form className="panel" onSubmit={changePassword}>
        <div className="row wrap">
          <label className="field grow">
            <span>Current password</span>
            <input
              type="password"
              value={pwCurrent}
              onChange={(e) => setPwCurrent(e.target.value)}
            />
          </label>
          <label className="field grow">
            <span>New password</span>
            <input
              type="password"
              value={pwNew}
              onChange={(e) => setPwNew(e.target.value)}
            />
          </label>
          <button style={{ alignSelf: "center" }}>Change</button>
        </div>
        {pwMsg && <div className="dim">{pwMsg}</div>}
      </form>

      {pickerFor && (
        <FolderPicker
          initial={cfg[pickerFor] ?? ""}
          onPick={(p) => {
            set({ [pickerFor]: p } as Partial<ServerConfig>);
            setPickerFor(null);
          }}
          onClose={() => setPickerFor(null)}
        />
      )}
    </>
  );
}
