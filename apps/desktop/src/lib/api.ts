// Typed invoke wrappers for the BLT desktop app (the Rust command surface).

import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";

// ── shared types (mirrors blt-core protocol + commands.rs DTOs) ──

export type Mode = "client" | "playback";

export interface Settings {
  mode: Mode;
  playback_locked: boolean;
  display_name: string;
  client_id: string;
  default_download_root: string | null;
  upload_cap_bytes_per_sec: number;
  share_back: boolean;
  last_server: string | null;
}

export interface ConnectionState {
  game_endpoint: string | null;
  share_endpoint: string | null;
  server_label: string | null;
  server_uuid: string | null;
  ws_connected: boolean;
}

export interface AppBootState {
  settings: Settings;
  connection: ConnectionState;
  mode_chosen: boolean;
  version: string;
}

export interface ServerRow {
  id: number;
  uuid: string | null;
  label: string | null;
  game_endpoint: string | null;
  share_endpoint: string | null;
  last_seen: number;
}

export interface LaunchEntry {
  name: string;
  exe: string;
  args?: string;
  cwd?: string;
}

export interface TitleInfo {
  name?: string;
  year?: number;
  genre?: string;
  players?: string;
  blurb?: string;
  link?: string;
  cover_b64?: string;
  launch?: LaunchEntry[];
  install_script?: { windows?: string };
}

export interface Title {
  id: number;
  name: string;
  label: string | null;
  total_size: number;
  file_count: number;
  manifest_ver: number;
  state: string;
  last_scan: number;
  info_hash: string | null;
  has_cover: boolean;
  has_install_script: boolean;
  local_state: "not_downloaded" | "partial" | "complete" | "update_available";
  local_dest: string | null;
  local_ver: number | null;
}

export interface DownloadPlan {
  dest: string;
  needed_bytes: number;
  available_bytes: number | null;
  enough_space: boolean;
}

export interface QueueEntry {
  title_id: number;
  manifest_ver: number;
  name: string;
  dest: string;
  status: string;
  total_chunks: number;
  have_chunks: number;
  bytes_total: number;
  bytes_done: number;
  speed_bps: number;
  error: string | null;
}

export interface TransferRow {
  id: string;
  kind: "game-download" | "share-download" | "share-upload";
  label: string;
  done: number;
  total: number;
  speed_bps: number;
}

export interface ValidationOut {
  all_ok: boolean;
  ok_count: number;
  total: number;
  failures: [string, string][];
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

export interface ShareListing {
  share: ShareSummary;
  files: { rel_path: string; size: number }[];
}

export interface ShareDownloadOut {
  total: number;
  present: number;
  missing: string[];
}

export type ItemType = "youtube" | "direct_url" | "shared_file" | "external";

export interface JukeboxItem {
  id: number;
  type: ItemType;
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

export interface RosterEntry {
  client_id: string;
  display_name: string;
  machine_name: string;
  activity: string;
  server_only: boolean;
  throughput_bps: number | null;
}

export interface ScriptPreview {
  contents: string;
  hash: string;
  runnable_here: boolean;
}

export interface ScriptResult {
  success: boolean;
  exit_code: number | null;
  output: string;
}

export interface UpdateInfo {
  version: string;
  notes: string | null;
}

// ── commands ──

export const api = {
  getAppState: () => invoke<AppBootState>("get_app_state"),
  chooseMode: (mode: Mode) => invoke<void>("choose_mode", { mode }),
  updateSettings: (patch: {
    display_name?: string;
    default_download_root?: string;
    upload_cap_bytes_per_sec?: number;
    share_back?: boolean;
  }) => invoke<Settings>("update_settings", { patch }),

  listServers: () => invoke<ServerRow[]>("list_servers"),
  connectTo: (gameEndpoint: string, shareEndpoint?: string | null, label?: string | null) =>
    invoke<void>("connect_to", { gameEndpoint, shareEndpoint, label }),
  connectionState: () => invoke<ConnectionState>("connection_state"),

  fetchTitles: () => invoke<Title[]>("fetch_titles"),
  fetchTitleInfo: (titleId: number, infoHash: string | null) =>
    invoke<TitleInfo>("fetch_title_info", { titleId, infoHash }),
  prepareDownload: (
    titleId: number,
    titleName: string,
    totalSize: number,
    destOverride?: string | null,
  ) =>
    invoke<DownloadPlan>("prepare_download", {
      titleId,
      titleName,
      totalSize,
      destOverride,
    }),
  beginDownload: (titleId: number, manifestVer: number, titleName: string, dest: string) =>
    invoke<void>("begin_download", { titleId, manifestVer, titleName, dest }),
  pauseDownload: (titleId: number) => invoke<void>("pause_download", { titleId }),
  resumeDownload: (titleId: number, manifestVer: number, titleName: string) =>
    invoke<void>("resume_download", { titleId, manifestVer, titleName }),
  cancelDownload: (titleId: number) => invoke<void>("cancel_download", { titleId }),
  deleteGame: (titleId: number) => invoke<void>("delete_game", { titleId }),
  downloadsSnapshot: () => invoke<QueueEntry[]>("downloads_snapshot"),
  activeTransfers: () => invoke<TransferRow[]>("active_transfers"),
  cancelTransfer: (id: string) => invoke<void>("cancel_transfer", { id }),
  validateTitle: (titleId: number, deep: boolean) =>
    invoke<ValidationOut>("validate_title", { titleId, deep }),
  repairTitle: (titleId: number, titleName: string) =>
    invoke<number>("repair_title", { titleId, titleName }),
  launchTitle: (titleId: number, infoHash: string | null, entryIndex: number) =>
    invoke<void>("launch_title", { titleId, infoHash, entryIndex }),

  scriptPreview: (titleId: number) => invoke<ScriptPreview | null>("script_preview", { titleId }),
  scriptRun: (titleId: number, expectedHash: string) =>
    invoke<ScriptResult>("script_run", { titleId, expectedHash }),

  sharesList: () => invoke<ShareSummary[]>("shares_list"),
  shareListing: (shareId: number) => invoke<ShareListing>("share_listing", { shareId }),
  shareUpload: (paths: string[]) => invoke<number>("share_upload", { paths }),
  shareDownload: (shareId: number, destDir: string, onlyMissing: boolean) =>
    invoke<ShareDownloadOut>("share_download", { shareId, destDir, onlyMissing }),
  shareDelete: (shareId: number) => invoke<void>("share_delete", { shareId }),
  preflightDest: (dest: string, needed: number) =>
    invoke<[boolean, number | null]>("preflight_dest", { dest, needed }),

  jukeboxState: () => invoke<JukeboxState | null>("jukebox_state"),
  jukeboxAdd: (itemType: ItemType, reference: string, title?: string | null) =>
    invoke<void>("jukebox_add", { itemType, reference, title }),
  jukeboxVote: (itemId: number) => invoke<void>("jukebox_vote", { itemId }),
  jukeboxNext: () => invoke<void>("jukebox_next"),
  jukeboxEnded: () => invoke<void>("jukebox_ended"),
  mediaProxyPort: () => invoke<number | null>("media_proxy_port"),
  roster: () => invoke<RosterEntry[]>("roster"),
  resolveShareStream: (shareId: number) => invoke<string>("resolve_share_stream", { shareId }),

  lockdownEnter: (password: string) => invoke<void>("lockdown_enter", { password }),
  lockdownExit: (password: string) => invoke<void>("lockdown_exit", { password }),
  logTail: (lines: number) => invoke<string[]>("log_tail", { lines }),
  externalOpen: (url: string) => invoke<void>("external_open", { url }),
  updateCheck: () => invoke<UpdateInfo | null>("update_check"),
  updateInstall: () => invoke<void>("update_install"),
};

/** Subscribe to a backend event; returns the unlisten promise. */
export function on(event: string, handler: () => void): Promise<UnlistenFn> {
  return listen(event, handler);
}

// ── formatting helpers ──

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

export function formatSpeed(bps: number): string {
  return `${formatBytes(bps)}/s`;
}
