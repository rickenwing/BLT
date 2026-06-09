# Buttz LAN Tool — Design Docs

Self-hosted, LAN-only game distribution + shared file pool + networked video jukebox for trusted friends at a LAN party. Cross-platform (Windows + macOS), built on **Tauri + a shared Rust core + Axum server + SQLite**, with **P2P-accelerated distribution** and **mDNS discovery**.

**Product:** Buttz LAN Tool (**BLT** everywhere machine-facing). Binary installs to Program Files / Applications; all runtime data (config, DB, backups, cache, logs) lives in a single configurable data root (`%LOCALAPPDATA%\BLT\` / `~/Library/Application Support/BLT/`) with `server/` and `client/` subfolders — see TDD §17.

## Read in order

1. **[01_PRODUCT_PLAN.md](01_PRODUCT_PLAN.md)** — vision, personas, scope, MVP, milestone roadmap (M0–M8), risks, the DRM reality.
2. **[02_FEATURE_SPEC.md](02_FEATURE_SPEC.md)** — every feature as user stories + testable acceptance criteria, tagged by milestone.
3. **[03_TECHNICAL_DESIGN.md](03_TECHNICAL_DESIGN.md)** — architecture, repo layout, SQLite schemas, manifest/chunk format, HTTP+WS+P2P protocol, NIC binding, jukebox state machine, self-update, and the sequenced implementation plan.

## TL;DR of decisions

- **Stack:** Tauri 2 (Rust + web UI) client/playback app; standalone Axum Rust server; shared `core` crate; SQLite both sides; BLAKE3 hashing; mDNS discovery; bespoke chunk-based P2P (server = preferred seed, clients secondary @ 1.5 MB/s cap default).
- **Server:** dedicated, headless, tray + password-protected web admin panel; **3 independently bindable services** (game distribution / shared pool / admin) across up to 3 NICs.
- **Distribution:** **unzipped** folder-per-title library (no zip/extract — preserves chunk-level resume, diff, and P2P; avoids double storage), scanned into manifests of 4 MiB chunks; chunked download with pause/resume/retry; quick validation (presence+size, chunks verified on arrival) + deep verify (full re-hash). Optional `.blt/` metadata sidecar gives **cover art + game info** via a separate cached info payload (fallback when absent).
- **Shared pool:** separate drive/NIC, server-direct, **files + folders** (drag-and-drop, structure-preserving, quick "X of N" completeness check), uploader-or-admin delete, persistent, display-name attributed.
- **Presence & routing:** live roster (who's online, activity, measured seed speed); reachability self-test with graceful server-only fallback; throughput-weighted scheduling (band-agnostic — slow peers just measure slower).
- **Jukebox:** upvote-only shared queue; embedded auto-advancing playback (YouTube / direct URL / shared-file LAN stream); **external-launch lane** for DRM (opens real browser/app, human presses Next). Only the playback machine renders media.
- **DRM:** embedded Widevine is **not** pursued (licensing wall, not a bypass); external launch is the sanctioned v1 path; embedded slot documented for the future.
- **Updates:** Tauri updater → GitHub Releases, Ed25519-signed; internet only at update time; airgap otherwise.

## Suggested first step for Claude Code

Start at **M0** in `03_TECHNICAL_DESIGN.md` §12: scaffold the workspace (`crates/core`, `crates/server`, `apps/desktop`), the mode switch, and CI with signing + a GitHub Releases workflow. Then proceed milestone by milestone, using the matching F-number acceptance criteria in `02_FEATURE_SPEC.md` as each PR's test checklist.
