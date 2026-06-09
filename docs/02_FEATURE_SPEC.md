# Buttz LAN Tool — Feature Specification

> **Document 2 of 3.** Companion docs: `01_PRODUCT_PLAN.md`, `03_TECHNICAL_DESIGN.md`.
>
> Each feature is written as user stories + **acceptance criteria (AC)**. AC are deliberately testable so each maps to a Claude Code work unit. Milestone tags (M0–M8) match the Product Plan roadmap.

---

## Conventions

- **Server** = the dedicated headless binary. **Admin panel** = its web UI. **Client** = the Tauri desktop app in client mode. **Playback** = the Tauri app in playback mode.
- "Title" = one canonical game = a folder tree of many files, distributed as a unit.
- "Share" = a file in the shared pool (no sync/diff).
- "Item" = an entry in the jukebox queue.

---

## F0. Repo, build & release  *(M0, M8)*

**Story:** As the admin, I want signed cross-platform builds produced from GitHub so I can install and later update the app safely.

**AC:**
1. Monorepo builds three artifacts: server binary (Win+Mac), Tauri client/playback app (Win+Mac), shared core crate consumed by both.
2. CI builds on push; tagged releases produce signed bundles published to GitHub Releases.
3. Update bundles are Ed25519-signed; the private key lives only in CI secrets, never in the repo.
4. **Manual self-update:** the app checks GitHub Releases when internet is reachable and, if a newer signed version exists, shows an **"update available"** indicator plus a **"Download & restart"** button. It **never auto-downloads, auto-installs, or auto-restarts.** Signature is verified before install; offline check is a graceful no-op. This applies to all modes and is especially important on a locked playback client (no surprise restart mid-party). *(M8)*
5. First run lets the user pick **client** or **playback** mode; changeable later in settings. *(M8)*

---

## F1. Server configuration & NIC binding  *(M1, M8)*

**Story:** As the admin, I want to bind each server service to a specific NIC (or all) so game traffic, file-share traffic, and admin traffic use the intended interfaces.

**AC:**
1. Server exposes three independent bind settings: **game-distribution**, **shared-pool**, **admin-panel** — each settable to a specific local interface/address or "all interfaces."
2. All three may collapse onto one NIC or spread across up to three; any combination is valid.
3. Bindings are editable in the admin panel and persisted (SQLite + config file); changing a binding restarts only the affected listener (or prompts a restart) without data loss.
4. The admin panel is reachable on its configured bind; if locked out by a bad binding, a documented config-file edit + restart recovers it.
5. Server library path (canonical games) and shared-pool path (separate drive) are independently configurable.
6. Admin panel requires a **single shared password** to access (set on first run, changeable). *(M8)*

---

## F2. Canonical library: scan, manifest, publish  *(M1)*

**Story:** As the admin, I want to drop a game folder into the library and have it become a downloadable title.

**AC:**
1. Admin sets a **library directory**; each immediate subfolder = one title (title name = folder name, editable label in panel).
2. **Staging workflow:** the admin places/copies a new or updated game into a **configurable staging path** (defaulted to the **same volume** as the library so promotion is an atomic rename). The server only **promotes** a staged title into the live library — and scans/publishes it — once it is **stable** (no file size/mtime changes for a configurable settle period, default ~30s), so a still-copying game is never published half-complete.
3. **Optional metadata sidecar:** a title folder *may* contain a `.blt/` sidecar with `info.json` (display name, year, genre, players, blurb, optional link) and a `cover.*` image. **All fields are optional with graceful fallback:** no `info.json` → display name = folder name; no cover → placeholder tile. The sidecar is the admin's *input* format only.
4. **Two separate payloads** per title, built during scan and persisted (SQLite):
   - a **game manifest** — every file with relative path, size, and content hash; large files split into chunks (chunk hashes recorded). The `.blt/` sidecar is **excluded** from the manifest's distributable file/chunk list (players download the game, not the metadata).
   - a **title-info payload** — the metadata fields plus the **cover embedded as base64**, with an `info_hash`. This is fetched/cached independently of the manifest so cover/info changes never touch the structural manifest and vice-versa.
5. **Scan now** button + configurable **periodic auto-scan** (default e.g. every N minutes) detect added/changed/removed titles.
6. Re-scan detects changed files (by size+mtime fast check, hashing only changed files) and republishes an updated **manifest version** (versions are immutable snapshots; see F4 for in-flight behavior). Changing only the sidecar updates the **info payload / `info_hash`** without bumping the manifest version.
7. Admin panel lists titles with: name, file count, total size, manifest version, publish state, last scan time, and whether metadata/cover is present.
8. A title mid-scan is not advertised as downloadable until its manifest is complete.

---

## F3. Client connect & discovery  *(M2 manual, M3 mDNS)*

**Story:** As a player, I want my client to find the server automatically, or let me type its address.

**AC:**
1. Client discovers the server via **mDNS**, showing discovered server(s) with their advertised **per-service endpoints** (game-distribution + shared-pool). *(M3)*
2. Client also accepts **manual IP/hostname** entry, always available as fallback. *(M2)*
3. Client auto-routes to the correct endpoint per service based on the advertisement (the player never needs to know which NIC is which). *(M3)*
4. Connection state (connected/disconnected/last server) is visible and remembered between launches.

---

## F4. Download canonical titles  *(M2 server-direct, M4 P2P)*

**Story:** As a player, I want to download all titles or pick specific ones, set where each lands, and survive Wi-Fi drops.

**AC — selection & location:**
1. Client lists available titles with **cover art and game info** (name, year, genre, players, blurb) drawn from the title-info payload, plus size and local state (not downloaded / partial / complete / out-of-date vs server manifest version). **Fallback:** no info → folder name + size; no cover → placeholder tile. The info payload is fetched when the browse UI renders and **cached locally, re-fetched only when `info_hash` changes**.
2. Player can **download all** or select **individual titles**.
3. Player sets a **global default download root**; per-title, player may override the destination folder. Chosen locations persist (client SQLite).
4. **Free-space pre-flight check:** before starting, the client compares the title's total size against free space on the destination volume. If insufficient, it **warns and asks for confirmation** (does not hard-block — the player may proceed knowing they'll free space).

**AC — transfer:**
5. Downloads are **chunked**; progress shown per title (and optionally per file).
6. **Pause** and **resume** any download; partially transferred chunks are not refetched after resume.
7. **Automatic retry** on transient network errors with backoff; a download survives a Wi-Fi drop and continues when connectivity returns.
8. Every received chunk is verified against its manifest hash; a failed chunk is refetched, not written.
9. **Manifest version change mid-download:** if the server republishes a title (new manifest version) while a download is in progress, the in-flight download **finishes against the version it started on** (keyed by `(title_id, manifest_ver)`), then the title is flagged **"update available."** No mixing of versions.

**AC — P2P:**  *(M4)*
10. A title downloads from the **server (preferred seed, uncapped)** plus any **peers** that already hold needed chunks.
11. Clients act as **secondary seeds**, serving chunks they hold; client upload is rate-capped (**default 1.5 MB/s**, configurable; server uncapped).
12. A **share-back toggle** (default **on**) lets a client become leech-only.
13. Peer-sourced chunks are hash-verified identically to server chunks; a bad peer cannot corrupt a title.
14. With two concurrent downloaders, server bandwidth is shared and peers offload some chunks (observable, not a hard quota).

---

## F5. Validation  *(quick: M2, deep: M3)*

**Story:** As a player, I want to confirm a title transferred completely and, when paranoid, that every byte is correct.

**AC:**
1. **Quick validation** (default, fast): for each file in the manifest, verify it exists locally with the expected size; rely on per-chunk hashes verified at arrival time. Reports any missing/short files.
2. **Deep verify** (explicit action): re-hash all local files for a title and compare to the manifest; reports per-file pass/fail.
3. Validation results are shown clearly; a failed title can be repaired by re-downloading only the missing/mismatched chunks.

---

## F6. Shared file pool  *(M5)*

**Story:** As a player, I want to share arbitrary files *and folders* with the group and grab what others shared, without sync.

**AC — sharing:**
1. The shared pool lives on the server's **separate shared-pool path/NIC**.
2. A share is a **file OR a folder tree**. Players can **upload** one or more files and/or folders (with explicit confirmation before the upload starts); uploads are attributed to the player's **display name**.
3. **Drag-and-drop:** dropping files and/or folders onto the shared-pool area initiates the upload flow (still gated by the confirmation step before bytes move). Native OS file-drop.
4. A folder-share preserves its directory structure on the share drive and is recorded as **one logical share** with a `kind` (file/folder), total size, and **file count**.

**AC — browsing & downloading:**
5. Players **browse** all shares (name, kind, size, file count for folders, owner display name, date) and **download** any share to a chosen location (with confirmation). Folder-shares display as an expandable tree.
6. **Free-space pre-flight check** (same as F4.4): before a share download, the client compares share size to free space on the destination volume and **warns + asks for confirmation** if insufficient (does not hard-block).
7. Downloading a folder-share **recreates the directory structure** at the chosen location.
8. **Completeness check (quick, no hashing):** after a download, the client verifies every expected file is present at the expected size and shows an **"X of N files"** count. A folder that didn't fully transfer (e.g. Wi-Fi drop) is visibly **incomplete**, and missing files can be re-fetched. No hashing/diff/sync beyond this.

**AC — lifecycle:**
9. **Delete** is allowed only by the **uploader or the admin**; others cannot delete. Deleting a folder-share removes the whole tree. Deletion requires explicit confirmation.
10. Shares **persist across parties** (server SQLite + files on the share drive).
11. Shared-pool transfers are **server-direct** (no P2P).

---

## F7. Display names  *(M5)*

**Story:** As a player, I want a recognizable name attributed to my shares and video adds.

**AC:**
1. On first run the client proposes a display name defaulted from the **computer name**.
2. The player can **edit** the display name in the client GUI; it persists and is sent with shares and jukebox adds.
3. Names are advisory labels only (no auth); the server stores the current mapping.

---

## F8. Video jukebox — queue & voting  *(M6)*

**Story:** As a player, I want to add videos to a shared queue and upvote so popular picks rise.

**AC:**
1. Any client can **add** an item to the queue. Item types: **YouTube link**, **direct video URL**, **shared-pool local file**, **external/DRM link** (Netflix/Hulu/Prime/etc.).
2. **Adding a shared-pool file:** the add UI lets the client **browse/pick from the shared pool** (selecting a video file from the existing share list) rather than entering an ID by hand; the queue item stores the resolved share reference so the playback machine can stream it.
3. Each item shows: title/source, type, who added it (display name), and **upvote count**.
4. **Upvote-only** voting (no downvotes, no skip). Each client may upvote a given item at most once (toggle off allowed). **Votes are keyed on `client_id`, not display name** — renaming never grants a second vote.
5. **Two ordering modes, admin-selectable** in the panel; the **default is Fair Rotation**:
   - **Fair Rotation (default):** the up-next order **round-robins by contributor** (`client_id`) so no one can hog the queue — if A added three and B added one, they interleave A, B, A, A. **Within a contributor's turn, their highest-voted item comes first** (ties → older add-time). Contributors with nothing queued are skipped; people joining/leaving mid-party are handled naturally.
   - **Vote-Ranked:** classic popularity — strictly `ORDER BY votes DESC, added_at ASC`, ignoring contributor.
6. In **both** modes the **currently-playing item is pinned** and never reordered out from under playback; votes/adds re-rank only the up-next list.
7. Clients see a **read-only now-playing + up-next + vote** view and can add/vote, but **cannot edit/remove/reorder or change the mode** — only the admin panel (and playback machine controls) can.
8. Queue, votes, item metadata, and the current ordering mode persist on the server (SQLite).

---

## F9. Video jukebox — embedded playback  *(M6)*

**Story:** As the room, I want the playback machine to play queued non-DRM videos automatically.

**AC:**
1. **Only the playback machine** renders video and audio; clients never render media.
2. Playback supports: **YouTube** (embedded player), **direct video URLs**, and **shared-pool local files streamed from the server/peer over the LAN** (range requests + read-ahead buffer; no full pre-download required).
3. On an embedded item **ending**, the queue **auto-advances** to the next item per vote order.
4. Playback mode UI shows **now playing**, **up next**, and basic transport relevant to embedded items.
5. The playback machine pulls queue state from the server and reflects changes (adds, vote reorder) live.

---

## F10. Video jukebox — external / DRM lane  *(M7)*

**Story:** As the room, I want to play a Netflix/Hulu/Prime title on the big screen and have the queue resume cleanly afterward.

**AC:**
1. When the current item is an **external/DRM** type, the playback machine **opens the link in the real installed browser/app** (e.g. Chrome/Safari), fullscreen where possible.
2. On opening an external item the queue enters an **"awaiting human"** state and does **not** auto-advance.
3. Both the **playback machine** and the **admin panel** display a clear status: **"▶ Playing externally — press Next to continue."**
4. Pressing **Next** (from playback machine **or** admin panel) advances the queue: if the next item is embedded it autoplays; if it's another external item it opens that one and re-enters "awaiting human."
5. No attempt is made to detect external playback completion; advancement is purely human-driven for external items.
6. *(Future, not built)* If an embedded-DRM player is ever licensed, it slots in as a new embedded item type without changing the queue contract.

---

## F11. Admin panel — jukebox & moderation  *(M5 shares, M6–M7 jukebox)*

**Story:** As the admin, I want full control of the queue and the shared pool.

**AC:**
1. Admin can **add, remove, reorder, and Next** queue items, clear the queue, and **switch the ordering mode** (Fair Rotation ↔ Vote-Ranked).
2. Admin can **delete any share** in the pool.
3. Admin sees live now-playing/external status and can drive **Next** for external items.
4. All admin moderation actions are behind the admin password (F1.6).

---

## F12. Persistence & state  *(M1–M8, incremental)*

**AC:**
1. **Server SQLite** stores: config/bindings, titles + manifests + chunk maps, shares + ownership, jukebox queue + votes, display-name mappings.
2. **Client SQLite** stores: known server(s), per-title download locations, default root, resume state (which chunks have/haven't arrived), share-back toggle, upload cap, display name, mode.
3. State survives app/server restarts and persists across parties.

---

## F13. Peer presence, reachability & throughput  *(roster + reachability/throughput: M4; roster UI may trail into M5)*

**Story:** As a player or admin, I want to see who's connected and have the system quietly route P2P traffic toward the peers that are actually fast — so slow Wi-Fi peers don't bottleneck downloads and unreachable peers don't break them.

**AC — presence roster:**
1. A **roster panel** (in clients and the admin panel) lists each connected client: **display name**, **machine name**, **current activity/status** (e.g. idle / downloading *Title X* / seeding / server-only), and **measured throughput** as a seed.
2. The roster is **live-updated** over the existing WebSocket as clients join/leave and change activity.
3. No Wi-Fi band / connection-type detection is performed or shown — measured throughput is the signal.

**AC — reachability self-test:**
4. On announcing to the server, a client receives 1–2 other peers' addresses and **probes** them (lightweight connect / single-chunk request) to determine whether client-to-client P2P is possible on this network.
5. If probes **fail** (e.g. AP/client isolation, firewall, VPN), the client enters **"server-only mode"**: it is shown as such in its own UI and the roster, the scheduler skips it as both a peer source and destination, and it still downloads fully from the server (the always-on preferred seed). P2P failure degrades gracefully and **visibly** — never silently.

**AC — effective throughput & scheduling:**
6. The downloader measures **real per-peer delivery rate** (bytes/elapsed per received chunk) as a rolling/EWMA average — no synthetic benchmark.
7. The chunk scheduler **weights source selection by measured throughput**, with the **server as the always-available baseline**: faster peers receive more chunk requests; slow peers contribute less; peers below a floor or failing reachability contribute nothing and the scheduler leans on the server.
8. Band differences (2.4 / 5 / 6 GHz) are **never special-cased in logic** — they manifest only as a measured throughput number, which the scheduler handles uniformly.

---

## F14. Playback lockdown mode  *(M7, with M8 password gate)*

**Story:** As the admin, I want the TV/projector machine locked to playback-only so a guest can't fiddle with it, but I can still get in to change settings.

**AC:**
1. A client option marks the machine as a **dedicated playback client**; toggling it **restarts** the app into **lockdown mode**.
2. In lockdown mode the app is **playback-only**: it shows only now-playing, up-next, transport, and the "playing externally → Next" control. Game browsing/downloads and the shared-pool UI are **hidden/disabled** (the dedicated playback machine never downloads games or uses the shared pool).
3. **Entering and exiting** lockdown mode is gated by the **admin password** (reuses F1.6). Without the password, the machine cannot be taken out of lockdown.
4. A locked playback client **never auto-updates or auto-restarts** (per F0.4); the admin must explicitly trigger an update.

---

## F15. Resilience & edge states  *(spans M2–M8)*

**Story:** As anyone at the party, I want the app to survive dropped connections, sleeping laptops, and empty states without showing stale or broken UI.

**AC — live connection (WebSocket):**
1. The `/ws` live channel (jukebox, progress, peer registry, roster) **auto-reconnects with backoff** when it drops, and on reconnect performs a **state resync** so the client never shows stale queue/roster state after a Wi-Fi blip.
2. While disconnected, the UI shows a clear **"reconnecting…"** indicator rather than silently freezing.

**AC — peer dropout (#4):**
3. If a peer becomes unresponsive mid-serve (laptop sleeps/closes), the downloader **times out, drops that peer**, and refetches the affected chunk(s) elsewhere (server fallback). A dropped peer is removed from the roster.

**AC — shared-pool delete race (#5):**
4. If a share is deleted (by owner or admin) while another client is downloading it, the in-flight download **errors gracefully** with a clear message; it does not crash or leave a corrupt partial silently presented as complete.

**AC — empty / unreachable states (#8):**
5. Empty states (no titles, empty shared pool, empty queue, zero peers) render clear **"nothing here yet"** UI, not blank screens.
6. Server unreachable on launch → client shows a clear disconnected state with retry / manual-connect, not an error wall.

**AC — server data safety (#3):**
7. The server's SQLite uses **WAL mode** and takes a **periodic backup snapshot** (configurable interval) so a corruption or drive hiccup mid-party doesn't lose the titles/shares-ownership/jukebox/config state.

---

## F16. Post-install scripts & launch options  *(M6-area; depends on completed downloads + metadata)*

**Story:** As the admin, I want a game to optionally run a setup script after it downloads and to offer a Play button, so titles that need a tweak or a launch command are turnkey for players.

**AC — post-install script:**
1. A title may include an **optional Windows post-install script** in its `.blt/` sidecar (e.g. `.blt/install.ps1` / `.cmd`), named in `info.json`. **Windows only** — no Mac script path.
2. The script runs **on the client, after the title finishes downloading and passes validation**, in the title's destination folder.
3. Scripts run **only for canonical-library titles** — never for shared-pool files.
4. Before the first run, the client **shows that a script will run and lets the user view its contents**, and requires **explicit confirmation**. (Scripts are arbitrary code from the server; even on a trusted LAN this stays explicit.)
5. Script success/failure is surfaced in the UI and **logged** (F17); a failed script leaves the downloaded files intact and reports the error (it does not delete the title).
6. CD-key handling, registry tweaks, config edits, etc. are expected to be done **inside the post-install script** (no separate key-management feature).

**AC — launch options:**
7. `info.json` may include an optional **`launch`** block: one or more named entries, each with an executable path (relative to the install dir), optional `args`, and optional working dir (`cwd`).
8. When a launch block is defined, the client shows a **Play** button; **multiple entries** → a small menu (e.g. "Play", "Dedicated Server"). **No launch block → no Play button** (BLT simply leaves the ready-to-run folder in place).
9. Launching runs the specified executable on the client with the given args/cwd; failures are logged.

---

## F17. Logging & log viewers  *(threads M0–M8; viewers ~M8)*

**Story:** As the admin or a player, I want clear logs and a way to read them in-app so I can debug "why won't this download finish" on party night.

**AC:**
1. **Structured logging** (Rust `tracing`) to **rotating files** in each component's `logs/` folder (server and client separately), plus console output in dev. Retention by size/count so logs don't grow unbounded across many parties.
2. **Info level** covers normal operations (scan start/finish, title published, download start/complete, peer join/leave, jukebox advance, update check, script run); **warn/error** cover failures (chunk verify fail + refetch, peer timeout, WS reconnect, download/script error, free-space warning, delete-race).
3. **In-app log viewers:** the **client** shows its own log; the **admin panel** shows the **server** log. Read-only, tail-style, **level-filterable**. No cross-machine log shipping — each component views its own.
4. Logs never record secrets (admin password, etc.).

---

## Cross-cutting non-functional criteria

- **Performance:** client UI usable on ~10-year-old hardware; transfer/hash/P2P run in the compiled core off the UI thread.
- **Airgap:** all features except YouTube/streaming playback and self-update function with no internet.
- **Cross-platform path safety (#6):** all received shared-pool file/folder names are **sanitized on the writing side** — reject/transform path-traversal (`../`), characters illegal on the target OS, and over-length paths (e.g. Windows 260-char); handle case-insensitive collisions. A folder authored on macOS must write safely on Windows.
- **mDNS uses IPs, not `.local` names (#9):** discovery TXT records advertise **IP:port**, so clients never depend on mDNS *hostname resolution* (only service discovery) — avoids flaky `.local` resolution on older Windows.
- **Safety of transfers:** no chunk is written to disk unverified; no destructive action (delete, large upload, external launch) without explicit confirmation in the initiating UI.
- **Confirmation rule:** uploads, deletes, and "download all" each require an explicit user confirmation step before executing.
