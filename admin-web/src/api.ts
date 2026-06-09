// Thin fetch wrapper for the BLT admin API. Session rides an HttpOnly cookie;
// a 401 anywhere flips the app back to the login screen via the handler that
// App registers here.

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

let onUnauthorized: (() => void) | null = null;
export function setUnauthorizedHandler(fn: () => void) {
  onUnauthorized = fn;
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: body !== undefined ? { "Content-Type": "application/json" } : undefined,
    body: body !== undefined ? JSON.stringify(body) : undefined,
    credentials: "same-origin",
  });
  if (res.status === 401) {
    onUnauthorized?.();
  }
  if (!res.ok) {
    let msg = `${res.status}`;
    try {
      const j = await res.json();
      if (j.error) msg = j.error;
    } catch {
      /* not json */
    }
    throw new ApiError(res.status, msg);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

export const api = {
  get: <T>(path: string) => request<T>("GET", path),
  post: <T>(path: string, body?: unknown) => request<T>("POST", path, body),
  put: <T>(path: string, body?: unknown) => request<T>("PUT", path, body),
  delete: <T>(path: string) => request<T>("DELETE", path),
};

// ── Types mirrored from the server API ──

export interface AuthState {
  needs_setup: boolean;
  authed: boolean;
}

export interface Status {
  uuid: string;
  label: string;
  version: string;
  uptime_secs: number;
  connections: number;
  binds: { game_distribution: string; shared_pool: string; admin_panel: string };
  paths: { library: string | null; staging: string | null; share: string | null };
}

export interface InterfaceInfo {
  name: string;
  ip: string;
  is_loopback: boolean;
}

export interface ServerConfig {
  game_distribution_bind: string;
  shared_pool_bind: string;
  admin_panel_bind: string;
  library_path: string | null;
  staging_path: string | null;
  share_path: string | null;
  chunk_size: number;
  staging_settle_secs: number;
  scan_interval_secs: number;
  db_backup_interval_secs: number;
  peer_timeout_secs: number;
  jukebox_order_mode: string;
  server_label: string;
}

export interface TitleRow {
  id: number;
  name: string;
  label: string | null;
  manifest_ver: number;
  total_size: number;
  file_count: number;
  state: string;
  last_scan: number;
  has_metadata: boolean;
  has_cover: boolean;
  has_install_script: boolean;
}

export interface ScanSummary {
  scanned: number;
  published: string[];
  republished: string[];
  info_updated: string[];
  removed: string[];
  errors: string[];
}

export interface ShareSummary {
  id: number;
  name: string;
  kind: "file" | "folder";
  size: number;
  file_count: number;
  owner_name: string;
  created_at: number;
}

export interface JukeboxItem {
  id: number;
  type: "youtube" | "direct_url" | "shared_file" | "external";
  ref: string;
  title: string | null;
  added_by: string;
  added_by_id: string;
  added_at: number;
  votes: number;
  voted_by_me?: boolean;
  state: string;
}

export interface JukeboxState {
  mode: "fair" | "votes";
  playback_state: "idle" | "playing_embedded" | "playing_external";
  now_playing: JukeboxItem | null;
  up_next: JukeboxItem[];
}

// ── Helpers ──

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 100 ? 0 : 1)} ${units[i]}`;
}

export function formatWhen(unixSecs: number): string {
  if (!unixSecs) return "—";
  return new Date(unixSecs * 1000).toLocaleString();
}

export function formatUptime(secs: number): string {
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m ${secs % 60}s`;
}
