# Buttz LAN Tool — Product Plan

> **Product name:** **Buttz LAN Tool** — abbreviated **BLT** everywhere machine-facing (folders, binaries, service IDs). Friendly display name "Buttz LAN Tool"; data folder `BLT`; binaries `blt-server` / `blt`; macOS bundle id `com.buttz.blt`; mDNS service `_blt._tcp`.
>
> **Document 1 of 3.** Companion docs: `02_FEATURE_SPEC.md`, `03_TECHNICAL_DESIGN.md`.

---

## 1. Vision

A self-hosted, LAN-only application for distributing and managing game files at a LAN party among a small group of trusted friends, plus a shared file pool and a networked video "jukebox" for the room's main screen. It replaces the pain of Windows file shares and manual copying with a fast, resumable, peer-accelerated, cross-platform app that "just works" on an airgapped network.

The product is explicitly **personal/hobby scale**: trusted users, no internet dependency during use (except video links), no authentication on data services in v1.

## 2. Core principles

1. **LAN-first, airgap-friendly.** Everything except YouTube/streaming playback works with zero internet. The only internet touchpoint is the optional software self-update.
2. **Fast on bad hardware.** Must run acceptably on a 10-year-old laptop and a current Mac alike. Heavy work (hashing, chunking, P2P) lives in a compiled core, not the UI.
3. **Resumable and verifiable.** Multi-GB transfers over flaky Wi-Fi must pause, resume, retry, and prove they arrived intact.
4. **Server-authoritative, peer-accelerated.** The dedicated server is the source of truth and the preferred seed; clients are secondary seeds that lighten the load, never a correctness dependency.
5. **Trusted-network simplicity.** No per-user auth on data in v1 (admin panel excepted). Integrity comes from manifest hashes, not identity.

## 3. Personas

| Persona | Who | Primary needs |
|---|---|---|
| **Admin / Host** | Owns the dedicated server. | Curate canonical game library, manage NIC bindings, moderate shared pool, control the jukebox, run updates. Uses the **web admin panel**. |
| **Player** | A friend at the party on a laptop/desktop (often Wi-Fi). | Browse & download games (all or selective), resume interrupted downloads, validate, choose where each game lands, upload/grab files in the shared pool, queue & vote on videos. Uses the **desktop client app**. |
| **Playback machine** | The Mac (likely) wired to the TV/projector. | Pull the jukebox queue from the server, play embedded video (YouTube/local/direct), and launch DRM services externally on human cue. Runs the **desktop app in playback mode**. |

## 4. Product surfaces

| Surface | Tech | Audience |
|---|---|---|
| **Server** | Headless Rust binary (Axum) + system tray; configured via web admin panel. Runs on the dedicated multi-NIC machine. | Admin |
| **Web admin panel** | Served by the server; password-protected. | Admin |
| **Desktop client app** | Tauri (Rust core + web UI). One binary, runs in **client** or **playback** mode (mode chosen at first run / in settings). | Players + Playback machine |

> The server is a separate binary from the Tauri client but **shares a common Rust core crate** (manifest, chunking, hashing, transfer, P2P, protocol types). See TDD §2.

## 5. Scope

### In scope (v1 / MVP through P2P)

- Dedicated server with **per-service NIC binding** (game distribution / shared pool / admin panel — independently bindable, collapsible onto one NIC).
- **Canonical game distribution:** admin stages game folders (configurable staging path, same-volume atomic promote, stability/settle check so half-copied games never publish); server scans, builds manifests, publishes titles. Optional per-title metadata sidecar (`.blt/info.json` + cover) drives **cover art + game info in the client**, with graceful fallback when absent. Manual "Scan now" + periodic auto-scan. Manifest versions are immutable; in-flight downloads finish on their version then flag "update available."
- **Client download** of canonical titles: download all, or selective per-title; **free-space pre-flight (warn + confirm)**; **pause/resume/retry**; **quick validation** (presence + size + arrival-verified chunk hashes) and **deep verify** (full re-hash).
- **Per-title download location** + a global default download root, client-side.
- **P2P distributed download** for canonical titles (server = preferred seed, clients = secondary rate-capped seeds). Chunk-hash verified against server manifest.
- **Shared file pool:** separate drive/NIC; clients upload **files and folders** to share (**drag-and-drop** supported), browse others' shares, download them with a **quick "X of N files" completeness check** (no sync/diff). Ownership = uploader; only uploader or admin can delete. Persistent across parties.
- **Peer presence & smart routing:** live **roster** (who's online, activity, measured seed speed); **reachability self-test** with graceful "server-only" fallback when client-to-client P2P is blocked; **throughput-weighted** chunk scheduling so slow Wi-Fi peers don't bottleneck and bands never need special-casing.
- **Display names** per user (defaulted from computer name, editable in client GUI), attributed to shares and jukebox additions.
- **Video jukebox:** unified queue with **upvote-only voting** (more votes → higher); **auto-advance** for embedded items; **external-launch lane** for DRM services with human-driven Next. Only the playback machine renders video/audio; clients see metadata + can add/vote. Admin (panel) + playback machine can edit/Next.
- **Discovery:** mDNS primary + manual IP/hostname fallback, per-service endpoint advertisement.
- **Persistence:** SQLite on server (titles, shares, queue, votes, names) — WAL mode + periodic backup snapshot — and client (download paths, resume state).
- **Per-title automation:** optional **Windows post-install script** (run client-side after download+validation, canonical-only, confirmation-gated and viewable) and optional **launch options** in `info.json` (a Play button / menu). BLT distributes and optionally launches; it is not a full launcher.
- **Sequential download queue** (visible) for multiple/all titles.
- **Logging:** structured `tracing` to rotating files on server and client, with in-app log viewers (client sees client log; admin panel sees server log).
- **Resilience:** auto-reconnecting live channel with state resync; peer-dropout timeout/refetch; graceful empty/unreachable/delete-race states; cross-platform path sanitization.
- **Playback lockdown:** the dedicated playback machine can be locked to playback-only (no game downloads/shared pool); entering/exiting lockdown is gated by the admin password.
- **Self-update:** **manual** Tauri updater → GitHub Releases (internet-at-update-time only); shows "update available" + Download & restart, never auto-restarts.

### Out of scope (v1) — documented future extensions

- **Embedded DRM playback** (Netflix/Hulu/Prime inside the app). Blocked by Widevine licensing; only external-launch is supported. Architecture leaves a slot. See §7.
- **Authentication / trust validation** on data services and P2P peers. Trusted LAN assumed.
- **Mirrored video** to client screens (only the playback machine renders).
- **Server-hosted offline update mirror** (clients updating from the local server with no internet). Designed-for, not built.
- **Bandwidth shaping beyond the client upload cap** (server-side per-connection shaping is a config stub, off by default).
- **Mobile clients, web client, Linux as a supported target** (Linux may work but is untested/unsupported).

## 6. MVP definition & milestone roadmap

Milestones are sized for iterative handoff to Claude Code. Each has acceptance criteria in the Feature Spec. The TDD (§12) restates these with technical detail and dependency order.

| Milestone | Theme | Ships |
|---|---|---|
| **M0** | Skeleton & repo | Monorepo, shared core crate stub, server binary stub, Tauri app stub (mode switch), CI build for Win+Mac, signing keys, GitHub Releases pipeline. |
| **M1** | Manifests & library | Server library folder scan → manifest/chunk model in SQLite; admin panel lists titles; "Scan now" + periodic scan; per-service config (bind addresses) UI + persisted. |
| **M2** | Server distribution (no P2P) | HTTP chunk/file serving on the game-distribution bind; client connects (manual IP), browses titles, downloads a title server-direct with **pause/resume/retry**; per-title + default download paths; **quick validation**. |
| **M3** | Discovery & deep verify | mDNS per-service advertise/browse; manual fallback; **deep verify** (full re-hash) action; download-all vs selective. |
| **M4** | P2P | Peer swarm for canonical titles; server preferred seed; clients secondary seeds with **1.5 MB/s default upload cap**; per-chunk hash verification from peers; share-back toggle (default on). Reachability self-test (server-only fallback), per-peer throughput measurement, throughput-weighted scheduling, and presence roster. |
| **M5** | Shared pool | Separate-drive/NIC file share: **file + folder** upload/browse/download (drag-and-drop; structure-preserving; "X of N" completeness check); ownership + delete rules; display names (defaulted from hostname, editable); persistence. |
| **M6** | Jukebox (embedded) | Unified queue + upvote voting; embedded playback (YouTube, local/shared file via LAN stream, direct URL); auto-advance; playback-mode UI; client add/vote/metadata view; admin panel queue controls. |
| **M7** | Jukebox (external lane) | DRM/external items: playback Mac opens real Chrome/Safari fullscreen; queue enters "awaiting human" state; Next from playback machine or admin panel resumes. Clear "playing externally" status everywhere. |
| **M8** | Admin password + polish | Admin-panel login (simple shared password); playback lockdown password gate; tray UX; settings polish; manual self-update (Download & restart, no auto-restart) wired to GitHub Releases; first-run mode selection; config-recovery flag; docs. |

**MVP line:** M0–M5 is a genuinely usable product (distribution + P2P + shared pool). M6–M7 add the jukebox. M8 hardens. Ship incrementally; nothing after M2 is a hard dependency for the one before it except where noted in the TDD.

## 7. The DRM reality (documented plainly)

Embedding Widevine to play Netflix/Hulu/Prime inside the app is **not achievable at hobby scale** and is **not a DRM bypass we will attempt**:

- The Widevine CDM is Google-licensed binary code; shipping it requires a license agreement with Google granted to vetted products.
- Streaming services maintain allowlists of approved client identities; even a Widevine-bearing custom webview is refused (the gate is the *client identity*, not the login), so no settings/credentials file makes it work.

**Therefore v1 uses the only sanctioned path:** DRM titles are launched in the playback machine's *real, already-licensed* browser/app as an **external-launch queue item**, outside the auto-advancing flow, advanced by a human pressing **Next**. If a Widevine license is ever obtained, an embedded-DRM player can slot into the same queue model (TDD notes the extension point).

## 8. Key risks & mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Old hardware can't handle UI/transfer | Player can't participate | Tauri (OS webview, low RAM); compiled Rust transfer core; modest concurrency defaults. |
| Wi-Fi drops mid-multi-GB download | Frustration, wasted bandwidth | Chunked transfer + resume + retry + chunk-hash verify (M2/M4). |
| Multicast/mDNS blocked on network | Discovery fails | Admin controls the network; manual IP/hostname always available; (UDP-broadcast beacon is a documented fallback, not built unless needed). |
| Corrupt/incomplete peer chunk | Bad game files | Every chunk verified against server manifest hash before acceptance (M4). |
| DRM expectations unmet | Disappointment | Documented clearly here + external-launch lane delivers the realistic outcome. |
| Self-update injects bad build | Compromised clients | Ed25519-signed updates; updater verifies signature; private key never in repo. |
| Disk vs NIC bottleneck assumptions wrong | Slow transfers | NVMe RAID saturates 10GbE per host; client upload cap small; dynamic sharing emerges from scheduler + TCP — measured, not assumed. |

## 9. Definition of done (v1)

- Admin can stand up the server on the dedicated 3-NIC machine, bind each service to a chosen NIC, and curate a game library by dropping folders.
- A player on Wi-Fi can discover the server, download a 50GB title with one interruption and successfully resume, validate it quickly, and choose where it lands — and a second concurrent downloader visibly shares server bandwidth while peers offload some chunks.
- Players can share files into the pool and grab others' shares; names are attributed and persist across parties.
- The room can queue YouTube + a shared local video, upvote, auto-advance; and push a Netflix title that opens on the playback Mac, with the queue cleanly resuming on Next.
- Clients can manually self-update from GitHub Releases when internet is available (no surprise restarts).
