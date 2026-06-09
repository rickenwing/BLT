# Buttz LAN Tool — Technical Design Document

> **Document 3 of 3.** Companion docs: `01_PRODUCT_PLAN.md`, `02_FEATURE_SPEC.md`.
>
> This is the build blueprint for handoff to Claude Code. It is opinionated and sequenced. Where a default is given, take it unless there's a measured reason not to.

---

## 1. Stack summary

| Concern | Choice |
|---|---|
| Desktop client/playback | **Tauri 2** (Rust backend + web UI). OS webview keeps RAM low on old hardware. |
| Web UI framework | React + TypeScript + Vite (any lightweight choice fine; keep deps small). |
| Server | Standalone **Rust** binary using **Axum** (HTTP) + **tokio**; **tray-icon** crate for the system tray. |
| Shared logic | A **`core` Rust crate** consumed by both server and Tauri app (manifests, chunking, hashing, transfer protocol types, P2P, discovery). |
| Persistence | **SQLite** via `sqlx` (or `rusqlite`) on both server and client. |
| Hashing | **BLAKE3** (fast, parallel) for file + chunk hashes. |
| Discovery | **mDNS / DNS-SD** via `mdns-sd` crate. |
| Transport | HTTP/1.1 + range requests for bulk data; **WebSocket** for live state (queue, progress, peer coordination signaling). |
| P2P | Custom chunk-exchange over the core protocol; **server is preferred seed**, clients secondary. (libp2p is optional; a simpler bespoke TCP/WS chunk protocol is sufficient and easier to reason about — see §7.) |
| Updates | **Tauri updater** → GitHub Releases, Ed25519-signed. |
| Video (embedded) | YouTube IFrame API + HTML5 `<video>` for direct/LAN-streamed files, inside the playback-mode webview. |
| Video (external/DRM) | OS "open URL/app" via Tauri shell/opener. |

> **Why bespoke P2P over libp2p:** the network is a single trusted LAN with a known server. We don't need NAT traversal, DHT, or peer authentication. A small chunk-request protocol keeps the surface area Claude-Code-maintainable. libp2p remains a documented swap-in if requirements grow.

---

## 2. Repository layout (monorepo)

```
blt/
├─ Cargo.toml                  # workspace
├─ crates/
│  ├─ core/                    # shared: manifest, chunking, hashing, protocol, p2p, discovery
│  │   ├─ manifest.rs
│  │   ├─ chunking.rs
│  │   ├─ hashing.rs           # BLAKE3 helpers
│  │   ├─ protocol.rs          # request/response + WS message enums (serde)
│  │   ├─ transfer.rs          # chunk fetch/verify/write, resume bitmap
│  │   ├─ p2p.rs               # peer chunk exchange, seed/leech logic, rate cap
│  │   └─ discovery.rs         # mDNS advertise/browse
│  ├─ server/                  # Axum binary
│  │   ├─ main.rs              # tray + service supervisor
│  │   ├─ bindings.rs          # per-service NIC bind
│  │   ├─ library.rs           # scan/watch/manifest publish
│  │   ├─ share.rs             # shared pool
│  │   ├─ jukebox.rs           # queue + vote state machine
│  │   ├─ admin_api.rs         # admin panel REST + WS (password-gated)
│  │   ├─ data_api.rs          # game-distribution + share endpoints
│  │   └─ db.rs                # server SQLite
├─ apps/
│  └─ desktop/                 # Tauri app (client + playback modes)
│     ├─ src-tauri/            # Rust side: invokes core, holds client SQLite
│     │   ├─ main.rs
│     │   ├─ mode.rs           # client | playback
│     │   ├─ download.rs       # orchestrates core::transfer + core::p2p
│     │   ├─ playback.rs       # embedded + external launch
│     │   └─ db.rs             # client SQLite
│     └─ src/                  # React/TS UI
│        ├─ client/            # browse, download, validate, shares, jukebox add/vote
│        └─ playback/          # now playing, up next, transport, external status
├─ admin-web/                  # admin panel SPA (served by server)
├─ docs/                       # these three documents
└─ .github/workflows/          # CI: build, sign, release
```

---

## 3. Server architecture

### 3.1 Service supervisor & NIC binding

The server runs **three independent listeners**, each with its own configurable bind address:

| Service | Default port (suggested) | Binds to | Purpose |
|---|---|---|---|
| `game_distribution` | 7400 | configurable NIC / all | manifest fetch, chunk/file serving, P2P coordination |
| `shared_pool` | 7401 | configurable NIC / all | share upload/list/download |
| `admin_panel` | 7402 | configurable NIC / all | admin SPA + REST/WS, password-gated |

- Each listener is a separate Axum `Server` bound to its configured `SocketAddr`. Config in SQLite + a `config.toml` fallback (so a bad admin binding is recoverable by editing the file).
- Enumerate interfaces with the `if-addrs`/`local-ip-address` crate to populate the admin dropdowns.
- Changing a binding stops/restarts only that listener.

### 3.2 mDNS advertisement

- Advertise service type `_blt._tcp.local`.
- Publish **TXT records** carrying per-service host:port for `game_distribution` and `shared_pool` (admin panel is intentionally not auto-advertised — admin reaches it directly).
- A server instance UUID + display label in TXT so clients show a friendly server name.

### 3.3 Library scan → manifest (with staging)

- **Staging path** (configurable, defaulted to the **same volume** as the library): the admin copies new/updated titles here. The server watches staging and **promotes** a title into the live library only when it is **stable** — no `size`/`mtime` changes across a configurable settle window (default ~30s) — so a still-copying game is never scanned/published mid-copy. Same-volume staging makes promotion an **atomic rename**, not a slow cross-drive copy.
- **Watched directory** = library root; each immediate subdirectory is a title.
- Scan walks the title tree, computing per-file: `relative_path`, `size`, `mtime`, and `blake3` of contents. Large files split into **fixed chunks**; small files are single-chunk.
- **Change detection on re-scan:** compare `size + mtime` first; only re-hash files whose size/mtime changed. Bump `manifest_version` when anything changes.
- **Manifest versions are immutable snapshots.** A client's in-flight download is keyed by `(title_id, manifest_ver)` and always completes against the version it started on; a republish surfaces as "update available" rather than mutating the active download (see Feature Spec F4.9). *(v1 does not implement file-locking or deferred cleanup of superseded files — the mid-update-while-downloading window is rare and accepted; revisit if it proves a problem.)*
- Periodic auto-scan via interval task + manual "Scan now". Use a filesystem watcher (`notify` crate) opportunistically but treat scan as the source of truth.
- A title is only advertised as downloadable once its manifest is fully built.
- **Optional metadata sidecar** (`<title>/.blt/`): `info.json` (all fields optional — `name`, `year`, `genre`, `players`, `blurb`, `link`, plus `launch` and `install_script` blocks) + a `cover.*` image + optional Windows scripts (e.g. `install.ps1`). The scanner reads metadata into the `titles` row, builds the **title-info payload** (fields + base64 cover + launch/script refs + `info_hash`), and **excludes `.blt/` from the distributable manifest** — **except** that referenced scripts are delivered to the client so they can run post-install (see §5.2). Missing sidecar → fall back to folder name; missing cover → placeholder. Editing only the sidecar updates `info_hash` without bumping `manifest_ver`.

Example `.blt/info.json` (all keys optional):
```json
{ "name": "Some Game", "year": 2021, "genre": "Co-op shooter",
  "players": "1-4 local", "blurb": "Short description shown in the client.",
  "link": "https://optional",
  "launch": [
    { "name": "Play", "exe": "Game.exe", "args": "-windowed", "cwd": "." },
    { "name": "Dedicated Server", "exe": "DedicatedServer.exe" }
  ],
  "install_script": { "windows": "install.ps1" } }
```
- `launch` → optional; defines the client's **Play** button(s). Multiple entries → a small menu. No block → no Play button.
- `install_script.windows` → optional path (within `.blt/`) to a Windows script run **client-side, post-download, post-validation**, in the install dir. Windows only. See §5.2.

### 3.4 Chunking parameters (handles few-huge and many-tiny)

- **Chunk size: 4 MiB default.** Rationale: good resume granularity, reasonable manifest size for 500 GB (~128k chunks at 4 MiB), efficient range reads off NVMe.
- Files **≤ chunk size** are a single chunk (no padding). Manifests for many-tiny-file games stay compact because each small file is one chunk entry.
- Make chunk size a server config value (rebuild manifests if changed). Document the tradeoff: smaller = finer resume but larger manifests; larger = coarser resume.

---

## 4. Data model (SQLite)

### 4.1 Server DB

```sql
-- config / bindings
CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- e.g. game_distribution_bind, shared_pool_bind, admin_panel_bind,
--      library_path, staging_path, staging_settle_secs (default 30),
--      share_path, chunk_size, scan_interval_secs,
--      db_backup_interval_secs, peer_timeout_secs,
--      jukebox_order_mode (fair|votes, default fair),
--      admin_password_hash, server_uuid, server_label

CREATE TABLE titles (
  id            INTEGER PRIMARY KEY,
  name          TEXT NOT NULL,          -- folder name
  label         TEXT,                   -- editable display label (overrides info.json name)
  manifest_ver  INTEGER NOT NULL,
  total_size    INTEGER NOT NULL,
  file_count    INTEGER NOT NULL,
  state         TEXT NOT NULL,          -- scanning | published | removed
  last_scan     INTEGER NOT NULL,
  -- optional metadata (from .blt/info.json; all nullable, graceful fallback)
  meta_year     INTEGER,
  meta_genre    TEXT,
  meta_players  TEXT,                   -- e.g. "1-4 local co-op"
  meta_blurb    TEXT,
  meta_link     TEXT,
  cover_b64     TEXT,                   -- base64 cover, embedded in the info payload
  info_hash     BLOB                    -- hash of the title-info payload; client cache key
);

CREATE TABLE files (
  id        INTEGER PRIMARY KEY,
  title_id  INTEGER NOT NULL REFERENCES titles(id),
  rel_path  TEXT NOT NULL,
  size      INTEGER NOT NULL,
  mtime     INTEGER NOT NULL,
  hash      BLOB NOT NULL               -- blake3 of file
);

CREATE TABLE chunks (
  id        INTEGER PRIMARY KEY,
  file_id   INTEGER NOT NULL REFERENCES files(id),
  idx       INTEGER NOT NULL,           -- chunk index within file
  offset    INTEGER NOT NULL,
  size      INTEGER NOT NULL,
  hash      BLOB NOT NULL               -- blake3 of chunk
);

CREATE TABLE shares (
  id           INTEGER PRIMARY KEY,
  name         TEXT NOT NULL,          -- file name or folder name
  kind         TEXT NOT NULL,          -- file | folder
  size         INTEGER NOT NULL,       -- total bytes (sum of tree for folders)
  file_count   INTEGER NOT NULL,       -- 1 for file; N for folder tree
  owner_name   TEXT NOT NULL,          -- display name at upload time
  owner_client TEXT,                   -- client_id for uploader-delete checks
  stored_path  TEXT NOT NULL,          -- file path or folder root on share drive
  created_at   INTEGER NOT NULL
);

-- file listing for folder-shares (recreates tree on download; quick completeness check)
CREATE TABLE share_files (
  id        INTEGER PRIMARY KEY,
  share_id  INTEGER NOT NULL REFERENCES shares(id),
  rel_path  TEXT NOT NULL,
  size      INTEGER NOT NULL
);

CREATE TABLE jukebox_items (
  id          INTEGER PRIMARY KEY,
  type        TEXT NOT NULL,            -- youtube | direct_url | shared_file | external
  ref         TEXT NOT NULL,           -- url, or share_id (for shared_file)
  title       TEXT,                     -- resolved/display title
  added_by    TEXT NOT NULL,           -- display name (advisory label)
  added_by_id TEXT NOT NULL,           -- client_id of adder
  added_at    INTEGER NOT NULL,
  state       TEXT NOT NULL            -- queued | playing | played
);

CREATE TABLE jukebox_votes (
  item_id   INTEGER NOT NULL REFERENCES jukebox_items(id),
  voter_id  TEXT NOT NULL,             -- client_id (NOT display name — renaming can't double-vote)
  PRIMARY KEY (item_id, voter_id)
);

CREATE TABLE names (                    -- current display-name registry (advisory)
  client_id  TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
```

### 4.2 Client DB

```sql
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- mode (client|playback), playback_locked (bool, default false),
--      display_name, client_id (uuid),
--      default_download_root, upload_cap_bytes_per_sec (default 1_572_864),
--      share_back (bool, default true), last_server

CREATE TABLE servers (
  id        INTEGER PRIMARY KEY,
  uuid      TEXT,
  label     TEXT,
  game_endpoint   TEXT,
  share_endpoint  TEXT,
  last_seen INTEGER
);

CREATE TABLE title_locations (
  title_id  INTEGER PRIMARY KEY,        -- server title id
  dest_path TEXT NOT NULL               -- per-title override; else default root
);

CREATE TABLE download_state (
  title_id      INTEGER NOT NULL,
  manifest_ver  INTEGER NOT NULL,
  chunk_bitmap  BLOB NOT NULL,          -- 1 bit per chunk: have/have-not (resume)
  status        TEXT NOT NULL,          -- partial | complete | paused | error
  PRIMARY KEY (title_id, manifest_ver)
);
```

> The `chunk_bitmap` is the heart of resume: on resume, fetch only zero-bits; on chunk verify-success, set the bit and persist periodically.

---

## 5. Protocol (game distribution)

All bulk transfer is plain HTTP so it's debuggable with curl and resumable for free.

| Endpoint | Method | Purpose |
|---|---|---|
| `/titles` | GET | List titles: id, label, size, file_count, manifest_ver, **info_hash** (so the client knows if its cached info/cover is stale). |
| `/titles/{id}/info` | GET | **Title-info payload**: metadata fields + base64 cover. Separate from the manifest; client fetches on browse render and caches by `info_hash`. |
| `/titles/{id}/manifest` | GET | Full manifest (files + chunks + hashes), JSON or compact binary. Structural only — no metadata/cover. |
| `/titles/{id}/files/{file_id}` | GET (Range) | Stream a file or byte range (chunk fetch). |
| `/chunks/{file_id}/{idx}` | GET | Fetch one chunk (server seed path). |
| `/ws` | WS | Live: download progress fan-out, peer announce/lookup, jukebox state. |

- **Two payloads, decoupled:** the **manifest** is the structural hot-path index (parsed on every re-scan/diff/resume); the **info payload** carries metadata + embedded base64 cover and is fetched/cached independently. A cover/info change bumps `info_hash` only; a game-file change bumps `manifest_ver` only. The `.blt/` sidecar is the admin's input format and is **excluded** from the manifest.
- **Resume** uses HTTP Range on file endpoints, or per-chunk fetch; client drives based on its bitmap.
- **Verification:** client computes BLAKE3 of each received chunk and compares to the manifest chunk hash before setting the bitmap bit / writing into the destination file at the correct offset.

### 5.1 Shared pool endpoints (separate listener/NIC)

A share is a **file or a folder tree** (`kind` = `file`|`folder`). Folder-shares preserve structure on the share drive.

| Endpoint | Method | Purpose |
|---|---|---|
| `/shares` | GET | List shares (id, name, kind, size, file_count, owner, date). |
| `/shares` | POST | Upload a file, multiple files, or a folder tree (multipart/stream; folder entries carry relative paths). Records owner display name. |
| `/shares/{id}` | GET | For `file`: metadata. For `folder`: the file listing (relative paths + sizes) so the client can recreate the tree. |
| `/shares/{id}/files/{rel}` | GET (Range) | Download an individual file within a share by relative path (also used to **LAN-stream** a shared file to the playback machine via range requests). |
| `/shares/{id}` | DELETE | Delete the share (whole tree if folder). Server enforces uploader-or-admin; admin via panel session, uploader via client id match. |

- **Folder download** = fetch the listing, then fetch each file (Range-resumable). Client does a **quick completeness check** afterward: every listed file present at expected size → report **"X of N files"**; missing files re-fetchable. No hashing.

### 5.2 Post-install scripts & launch options (F16)

- **Endpoint:** `/titles/{id}/script` (GET) returns the title's Windows post-install script bytes (if defined), with its hash so the client can display/verify it. Scripts exist **only for canonical-library titles**; the shared pool has no script concept.
- **Delivery:** the `.blt/` sidecar is excluded from the distributable file/chunk manifest, but a referenced `install_script.windows` is fetched separately via the endpoint above when present.
- **Execution model (client-side):**
  1. Title finishes downloading and passes validation.
  2. If an install script is defined, the client **shows the user that a script will run, lets them view its full contents**, and requires **explicit confirmation** before running. (Arbitrary code from the server; explicit even on a trusted LAN.)
  3. On confirm, run via the OS shell (PowerShell/cmd) **in the title's install directory**. **Windows only** — on a non-Windows client the script is ignored (those clients aren't running the Windows game anyway).
  4. Capture exit code + output → surface success/failure in UI and **log** (§ logging). Failure leaves files intact and reports the error; it never deletes the title.
- **Launch options:** the `launch` block from `info.json` drives the client's **Play** button (single entry) or menu (multiple). Launching spawns `exe` with optional `args`/`cwd` relative to the install dir. No block → no Play button (BLT distributes; launching is opt-in per title). Launch failures are logged.
- **Safety:** scripts run **only from the canonical library**, never the shared pool; never auto-run without confirmation; never logged with secrets.

---

## 6. Validation logic

- **Quick (default):** for each manifest file, `stat` the local file; pass if it exists and size matches. Because every chunk was BLAKE3-verified at arrival, presence+size is a strong-enough completeness check without re-reading 500 GB.
- **Deep:** re-hash every local file with BLAKE3, compare to manifest file hash; report per-file. Repair = recompute which chunks differ and refetch only those (server or peer).

---

## 7. P2P design (M4)

**Model:** server is the **always-available preferred seed**; clients are **secondary seeds** for chunks they already hold.

- **Peer registry:** clients announce over the `/ws` channel which `(title_id, manifest_ver)` they have chunks for and a reachable host:port for their local chunk server. The server relays a lightweight peer list (it's the rendezvous; no DHT needed).
- **Client chunk server:** each client (when `share_back` is on) runs a tiny HTTP listener serving `/chunks/{file_id}/{idx}` for chunks present in its bitmap.
- **Rate cap:** client upload (seed) limited to `upload_cap_bytes_per_sec` (default **1.5 MB/s** = 1,572,864 B/s). Token-bucket in `core::p2p`. Server uncapped.
- **Share-back toggle off** = client never starts its chunk server; pure leech.

### 7.1 Reachability self-test (F13)

On announce, the server hands the joining client **1–2 peer addresses** to probe. The client attempts a lightweight connect + single-chunk (or HEAD) request to each:

- **Success** → P2P-capable; participates as source/destination.
- **Failure** (AP/client isolation, firewall, VPN) → client flags **`server_only`** in its registry state. Scheduler skips it both ways; it still downloads fully from the server. Surfaced in UI + roster so failure is **visible, not silent**.

This is cheap (registry/WS already exist) and converts LANBucket's silent P2P-isolation failure into a graceful, observable degradation.

### 7.2 Effective throughput measurement (F13)

The downloader records **real per-peer delivery rate** — `chunk_bytes / elapsed` — for every chunk received from each peer, kept as an **EWMA** (or rolling avg over last ~10–20 chunks) per peer in memory. No synthetic benchmark. This number *is* the peer's effective seed throughput and inherently captures Wi-Fi band, distance, and congestion. Reported to the roster over `/ws`.

> **Band-agnostic by design:** 2.4 / 5 / 6 GHz are never referenced in scheduling logic. A slow-band peer simply measures slower. The roster shows measured speed only — no band/connection-type detection (avoids per-OS plumbing and stays philosophically consistent).

### 7.3 Scheduler (downloader side, throughput-weighted)

1. Determine missing chunks from the bitmap.
2. **Server is the always-available baseline** and takes the bulk of requests.
3. Among reachable peers advertising needed chunks, **weight chunk-request assignment by measured EWMA throughput** — faster peers get more, slow peers fewer. Peers below a configurable floor, or flagged `server_only`/unreachable, get **zero** and the scheduler leans on the server.
4. Verify every chunk (server or peer) against the manifest hash before accepting. A failed verify blacklists that source for that chunk and refetches elsewhere (server fallback).
5. **Peer dropout (F15.3):** a chunk request to a peer that **times out** (laptop slept/closed/left) drops that peer from the active set, refetches the chunk from the server (or another peer), and removes the peer from the roster. Timeout is short and configurable; the server fallback guarantees progress regardless of how many peers vanish.

- **Dynamic sharing** is emergent: one downloader saturates from the 10GbE server; a second splits the server's stream via TCP fairness and additionally pulls overlapping chunks from peers, weighted toward the fast ones. No explicit quota engine in v1. A server-side per-connection cap exists as an **off-by-default** config stub.

> **Future swap-in:** replacing the bespoke peer protocol with libp2p touches only `core::p2p` and the peer-registry WS messages. The reachability/throughput/roster logic is transport-agnostic and would carry over.

### 7.4 Presence roster (F13)

The server tracks transient per-client session state (display name, machine name, activity, `server_only` flag, last-reported seed throughput) and fans it out over `/ws`. Clients and the admin panel render a live roster. Session/activity state is **transient (in-memory)**, not persisted; only the durable bits (display-name registry) live in SQLite.

---

## 8. Jukebox state machine (M6–M7)

**Server holds authoritative queue state.** Playback machine and clients subscribe via `/ws`.

States per the *current* item:
- `Idle` — empty queue / nothing playing.
- `PlayingEmbedded` — YouTube/direct/shared-file rendering on playback machine; on `ended` → auto-advance.
- `PlayingExternal` — external/DRM item; playback machine has launched the OS browser/app; queue is **awaiting human**; no auto-advance.
- Transitions:
  - **Add item** → inserted and the up-next list re-ranked per the **active ordering mode** (Fair Rotation or Vote-Ranked). Current item pinned.
  - **Vote** → re-rank up-next only (never reorders the pinned current item).
  - **ended (embedded)** → mark `played`, promote next, set state by next item's type.
  - **Next (human, from playback or admin)** → force-advance regardless of current type; primary way to leave `PlayingExternal`.
  - **Admin remove/reorder/clear** → mutate queue; if current removed, advance.

**Ordering — two modes (admin-selectable, persisted; default Fair Rotation):**
- **Fair Rotation (default):** up-next is a **round-robin over contributors keyed by `client_id`**; within each contributor's slot, pick their highest-voted not-yet-played item (ties → earliest `added_at`). Skip contributors with no queued items; recompute as people add/vote/join/leave. Prevents one person hogging the queue.
- **Vote-Ranked:** `ORDER BY vote_count DESC, added_at ASC` across all items, ignoring contributor.
- The mode is stored in `config`; switching it re-ranks the up-next list live over `/ws`. In **both** modes the currently-playing item is **pinned** and excluded from reordering.

**External launch:** Tauri `shell`/`opener` opens the URL on the playback host; fullscreen is best-effort (OS/browser dependent). UI everywhere shows "▶ Playing externally — press Next."

**Embedded playback:**
- YouTube → IFrame Player API in the playback webview; subscribe to player state for `ended`.
- Direct URL / shared file → HTML5 `<video>`. Shared files are **streamed** from `/shares/{id}/files/{rel}` (or a peer) via Range with read-ahead; no full pre-download.

**Adding a shared file (F8.2):** the client's add-to-queue UI lists shareable video files from the shared pool (`/shares`) and lets the user pick one; the queue item stores the `share_id` as its `ref`. The playback machine resolves that to the share's stream URL at play time.

**Live channel resilience (F15.1–.2):** the `/ws` connection (jukebox state, download progress, peer registry, roster) **auto-reconnects with backoff**. On reconnect the client requests a **full state resync** (current queue + now-playing + roster snapshot) so no stale state lingers after a drop. While down, the UI shows "reconnecting…". This applies to all `/ws` consumers, not just the jukebox.

---

## 9. Discovery details (M3)

- Server advertises `_blt._tcp` with TXT: `uuid`, `label`, `game=IP:port`, `share=IP:port`.
- **TXT records carry IP:port, not `.local` hostnames (F15 / #9)** — clients use mDNS only for service *discovery*, never for hostname *resolution*, avoiding flaky `.local` resolution on older Windows. The server fills in the IP of the relevant bound NIC per service.
- Client browses on startup + periodically; merges results into `servers` table; UI lists servers by label.
- Manual entry writes a `servers` row directly. Both paths converge on the same per-service endpoints.

---

## 10. Self-update (M8) — manual only

- `tauri-plugin-updater` configured with the GitHub Releases endpoint and the **public** Ed25519 key embedded in the app.
- CI signs bundles with the **private** key (GitHub Actions secret).
- Update check runs on launch when online and, if a newer signed version exists, surfaces an **"update available"** indicator + a **"Download & restart"** button. **No auto-download, auto-install, or auto-restart** — the user (admin) always initiates. Signature verified before install; offline check is a graceful no-op.
- **Locked playback clients** follow the same manual rule — never restart mid-party unless the admin explicitly triggers it.
- *(Documented future option: point the updater at the local server, which mirrors GitHub releases, for in-party airgapped updates. Not built in v1.)*

---

## 11. Security & safety posture (v1)

- **No data-service auth** (trusted LAN). Integrity from manifest hashes only.
- **Admin panel**: single shared password (argon2/bcrypt hash in `config`); session cookie. The only authenticated surface.
- **Playback lockdown**: the playback machine's lockdown mode is **entered and exited via the admin password** (reuses the same hash). Locked = playback-only, never downloads.
- **Confirmation gates** (UI-enforced): uploads, deletes, "download all", and external launches require explicit user confirmation.
- **Free-space pre-flight**: game and share downloads warn + confirm (not block) when the destination volume lacks space.
- **Cross-platform path safety:** shared-pool file/folder names are **sanitized on the writing side** — reject path-traversal (`../`), transform/refuse OS-illegal characters, enforce length limits (Windows ~260), resolve case-insensitive collisions. A folder authored on one OS must write safely on another.
- **Server data safety:** SQLite in **WAL mode** + **periodic backup snapshot** (configurable) so mid-party corruption doesn't lose titles/shares-ownership/jukebox/config.
- **Shared-pool delete race:** deleting a share that's being downloaded causes the in-flight download to **error gracefully** (no crash, no silent corrupt-complete).
- **No chunk written unverified.** Bad peer cannot corrupt titles.
- **Post-install scripts:** run **only from the canonical library** (never the shared pool), **confirmation-gated and viewable** before first run, Windows-only. Arbitrary server code is never auto-executed.
- **Updates signed**; private key never in repo. Updates are **manual** (no surprise restarts).
- **Config recovery**: server supports `config.toml` editing + restart to recover from a bad bind, plus a `--reset-admin-bind` CLI flag as a belt-and-suspenders unlock for the admin panel listener.
- Threats out of scope by design: malicious LAN peers beyond chunk-poisoning (prevented), auth/identity spoofing (no identity), DRM circumvention (not attempted).

### 11.1 Logging (F17)

- **Rust `tracing`** → **rotating files** in each component's `logs/` folder (server and client separate), plus console in dev. Rotation by size/count for bounded growth across parties.
- **Levels:** info for normal ops (scan, publish, download start/complete, peer join/leave, jukebox advance, update check, script run); warn/error for failures (chunk verify fail+refetch, peer timeout, WS reconnect, download/script/launch error, free-space warning, delete-race).
- **In-app viewers:** client shows its own log; admin panel shows the **server** log. Read-only, tail-style, level-filterable. No cross-machine shipping.
- **Never log secrets** (admin password, session tokens).

---

## 12. Implementation plan (sequenced for Claude Code)

Each milestone = one or more PRs with the Feature-Spec ACs as the test checklist. Dependencies noted.

| M | Deliverable | Depends on | Key crates/areas |
|---|---|---|---|
| **M0** | Workspace, `core` stub, server stub (tray + one dummy listener), Tauri app stub with mode switch, **data-root resolution (per-OS default + override) and `BLT/server` + `BLT/client` subfolder creation**, **`tracing` logging to rotating files (foundation, threaded through all later milestones)**, CI build Win+Mac, signing keys, release workflow. | — | tauri (path APIs), axum, tray-icon, tracing, GH Actions |
| **M1** | Library scan → manifest/chunks in server SQLite; **staging path + stability/settle promotion** (same-volume atomic rename); admin panel lists titles; Scan now + periodic; per-service bind config UI + persistence; interface enumeration. | M0 | notify, blake3, if-addrs, sqlx |
| **M2** | `game_distribution` HTTP serving; client manual-connect, browse titles, **server-direct chunked download** with pause/resume/retry; **sequential download queue (visible)**; per-title + default paths; **quick validation**; client SQLite + bitmap. | M1 | core::transfer, reqwest, range |
| **M3** | mDNS advertise/browse + manual fallback; **deep verify**; download-all vs selective; **title-info payload + cover art/game-info display** (metadata sidecar scan, `/titles/{id}/info`, client cache by `info_hash`, fallback). | M2 | mdns-sd |
| **M4** | **P2P**: peer registry over WS, client chunk server, scheduler (server-preferred + peers), per-chunk verify from peers, **1.5 MB/s upload cap**, share-back toggle. **Reachability self-test** (server-only fallback), **per-peer EWMA throughput**, **throughput-weighted scheduling**, and **presence roster** (display/machine name, activity, measured speed; roster UI may trail into M5). | M2 (M3 helpful) | core::p2p, tokio, token-bucket |
| **M5** | **Shared pool** on separate bind/drive: **file AND folder** upload/list/download (recursive, structure-preserving), **drag-and-drop** (files + folders), **quick completeness "X of N"** on folder downloads, ownership + delete rules, **display names** (hostname default + editable), persistence. **Post-install scripts** (Windows, confirm+view, canonical-only, post-validation) **and launch options / Play button** from `info.json`. | M1 (bindings), M2/M3 patterns | share.rs, multipart, tauri file-drop, shell |
| **M6** | **Jukebox embedded**: queue + upvote, ranking, client add/vote/read-only view, playback-mode embedded player (YouTube/direct/shared-file LAN stream), auto-advance; admin queue controls. | M5 (shared-file source), WS | jukebox.rs, IFrame API, HTML5 video |
| **M7** | **External/DRM lane**: open in real browser/app, awaiting-human state, Next from playback or admin, clear status UI. **Playback lockdown mode** (playback-only UI; enter/exit gated by admin password — M8 dependency for the password). | M6 (lockdown gate needs M8) | tauri shell/opener |
| **M8** | Admin **password login**, **playback lockdown password gate**, **in-app log viewers (client log; admin-panel server log, level-filterable)**, tray UX, settings polish, **manual self-update** (update-available + Download & restart, no auto-restart) → GitHub Releases, first-run mode pick, `--reset-admin-bind` recovery flag, **NSIS installer (Win) + DMG (Mac), un-notarized**, docs. | M0–M7 | updater plugin, argon2, NSIS, dmg |

**Suggested PR sizing for Claude Code:** one feature (F-number) per PR where possible; M4 and M6 may each need 2–3 PRs (registry+server, then scheduler; queue/vote, then player). Keep `core` changes in their own PRs so server and client pick them up cleanly.

---

## 13. Open implementation choices (safe to defer / let Claude Code pick)

- React state lib (likely none/Zustand) — keep minimal.
- Manifest wire format JSON vs binary (start JSON; add compact binary if 500 GB manifests get heavy).
- `sqlx` vs `rusqlite` — either; `rusqlite` is simpler for an embedded single-process DB.
- Exact periodic-scan interval, read-ahead buffer size, retry/backoff constants — start with sane values, tune by measurement.
- Whether the client chunk server reuses the same HTTP stack as downloads (recommended) or a minimal separate one.

---

## 14. Test/acceptance anchors (from Feature Spec)

- 50 GB title, one forced Wi-Fi drop → resume completes; quick-validate passes; deep-verify passes.
- Two concurrent downloaders → server bandwidth shared; peer offload observable; both complete and verify.
- Bad chunk injected by a peer → rejected, refetched from server, title still verifies.
- **Peer with client-isolation on** → reachability probe fails → client shows "server-only," scheduler skips it, download still completes from server.
- **Mixed-speed peers** (one fast, one slow) → fast peer measurably receives more chunk requests; slow peer contributes without bottlenecking; roster shows distinct measured speeds.
- **Folder-share** uploaded (via drag-drop) and downloaded by another client → directory structure recreated; "X of N files" completeness reported; a dropped transfer shows as incomplete and re-fetches missing files.
- **Staging:** a game still being copied into the staging folder is NOT published until it stops changing for the settle window; once stable, it promotes (atomic rename) and scans.
- **Free space:** starting a download larger than destination free space → warn + confirm (proceeds if confirmed, not hard-blocked); applies to game and share downloads.
- **Manifest version change:** server republishes a title mid-download → the in-flight download finishes on its original version, then shows "update available."
- **Playback lockdown:** toggling lockdown restarts into playback-only UI; exiting requires the admin password; the box never appears as a game-download peer.
- **WS resilience:** drop a client's `/ws` mid-party → it shows "reconnecting…", auto-reconnects, and resyncs queue/roster with no stale state.
- **Peer dropout:** a seeding peer sleeps mid-transfer → downloader times out, drops it, refetches from server, completes; peer leaves the roster.
- **Path safety:** a folder with an OS-illegal character (authored on macOS) downloads to Windows without failing — names sanitized on write.
- **Delete race:** delete a share while another client downloads it → that download errors gracefully, no crash, no corrupt-complete.
- **Vote integrity:** a client renames itself and tries to re-upvote the same item → still one vote (keyed on client_id).
- **Post-install script:** a title with a Windows `install.ps1` → after download+validation the client prompts (showing script contents), runs on confirm in the install dir, logs success/failure; declining skips it; shared-pool files never trigger scripts.
- **Launch options:** a title with a `launch` block shows a Play button (menu if multiple); launching spawns the right exe/args/cwd; a title without one shows no Play button.
- **Sequential queue:** queueing several titles downloads them one at a time with the queue visible.
- **Logs:** normal ops and failures appear in the rotating logs; the client log viewer and the admin-panel server-log viewer render and filter by level; no secrets present.
- Three NICs, services split across them → each reachable on its bind; mDNS routes clients correctly.
- Queue: YouTube + shared local file auto-advance; Netflix item opens externally, queue waits, Next resumes into the following item.
- **Fair queueing:** one contributor adds three items, another adds one → in Fair Rotation mode they interleave (no hogging), highest-voted-first within each contributor's turn; switching to Vote-Ranked mode re-orders strictly by votes.
- Offline launch → everything but YouTube/streaming + update-check works; update indicator stays silent offline; no auto-restart ever.

---

## 15. Deliberate deferrals (documented, not built in v1)

These are conscious scope choices, recorded so they're not mistaken for oversights and so the architecture leaves room for them:

- **Server-side bandwidth shaping** beyond the client upload cap. The client cap (default 1.5 MB/s) ships; finer per-connection/server-side shaping is an **off-by-default config stub** only. Dynamic fairness is left to the scheduler + TCP. Revisit only if a real bottleneck appears.
- **Embedded DRM playback** (Netflix/Hulu/Prime inside the app). Blocked by Widevine licensing; v1 uses the external-launch lane exclusively. The queue model leaves a clean slot for an embedded-DRM item type if a license is ever obtained.
- **Auth / identity on data services.** Trusted-LAN posture: no per-user auth on game/share/jukebox data. Integrity comes from manifest hashes. The admin panel + playback-lockdown gates are the only authenticated surfaces. A future auth/trust layer would sit in front of the data APIs and the P2P peer registry without changing the core protocol.

*(Other items explicitly out of scope per the Product Plan — server update mirror, mirrored video, shared-pool quotas, i18n/accessibility — remain out for v1.)*

---

## 16. Operational realities (setup README, not code)

To be captured in the eventual user/setup README rather than implemented:

- **Firewall prompts:** on first launch, Windows/macOS will prompt to allow the app's listeners. If denied, discovery and transfers silently fail. Document the "allow through firewall / set network profile to Private (Windows)" step prominently.
- **Pre-party network check:** confirm AP/client isolation is OFF and that two clients on different bands (2.4/5/6 GHz) can reach each other directly (a peer-to-peer ping), since P2P rides the client LAN.
- **Playback machine readiness:** the external-launch lane assumes the playback box already has Chrome/Safari (or the native apps) installed and **logged into** the streaming services beforehand.
- **mtime caveat:** scan change-detection uses size+mtime; copy tools that reset mtimes on the library can trigger unnecessary re-hashing. Prefer copy methods that preserve timestamps when updating the library.

---

## 17. Install & data layout (M0 + M8)

**Product name:** Buttz LAN Tool. **Machine-facing identifier:** `BLT`. Friendly display name "Buttz LAN Tool"; binaries `blt-server` and `blt` (client/playback); macOS bundle id `com.buttz.blt`.

### 17.1 App binary install (separate from data)

- **Windows:** Tauri builds an **NSIS `.exe`** installer (preferred over MSI for flexibility). App installs to `C:\Program Files\Buttz LAN Tool\`. Per-machine install.
- **macOS:** Tauri builds a **`.dmg`** delivery wrapper showing `Buttz LAN Tool.app` + an `/Applications` shortcut; the user drags the `.app` to `/Applications/`. The `.app` bundle (executable + resources) is the install.
- **No code signing / notarization in v1** (developer is the only Mac user). Un-notarized `.app` → first launch is **right-click → Open** to clear Gatekeeper once. Signing/notarization is a documented later step (needs an Apple Developer account) and slots into CI without code changes.
- The install location is **read-only at runtime** and is **replaced wholesale by the updater** — it must never hold runtime data.

### 17.2 Data root (config, DB, backups, cache, logs)

A **single configurable data root** holds everything the app writes, with per-OS defaults:

- **Windows default:** `%LOCALAPPDATA%\BLT\` (no admin needed, survives updates, per-user-correct).
- **macOS default:** `~/Library/Application Support/BLT/`.
- **Override:** a setting lets the data root be relocated to a visible path (e.g. `D:\BLT\` or `~/BLT/`) for those who prefer it out in the open. (`~/Library` is hidden in Finder by default — the override exists for exactly this preference.)

Server and client are separate binaries with separate state, so they use **subfolders under the one root** (keeps "everything in one place" while isolating each binary's state):

```
BLT/
├─ server/
│  ├─ config.toml
│  ├─ blt-server.db            (SQLite, WAL mode)
│  ├─ blt-server.db-wal / -shm
│  ├─ backups/                 (periodic DB snapshots — §11)
│  ├─ cache/
│  └─ logs/
└─ client/
   ├─ config.toml
   ├─ blt-client.db            (SQLite, WAL mode)
   ├─ blt-client.db-wal / -shm
   ├─ cache/                   (cover-art / title-info cache)
   └─ logs/
```

> Note: the server's **game library**, **staging path**, and **share path** are large data stores configured independently (they live on the dedicated server's NVMe/share drives), NOT under the `BLT/` data root. The data root holds only app state (DB/config/backups/cache/logs).

### 17.3 Rules

- Resolve the data root once at startup (default per-OS, or the configured override); create the subfolder tree if missing. Use Tauri's path APIs for the OS defaults.
- The updater replaces only the installed binary; it must never touch the data root.
- Uninstall leaves the data root in place by default (so config/DB survive a reinstall); document where it is so it can be removed manually if desired.
