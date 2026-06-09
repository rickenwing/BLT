# CLAUDE.md — Buttz LAN Tool

This file is auto-loaded every session. Read it fully before doing anything. Then read `docs/` before implementing any milestone.

## What this project is

A self-hosted, **LAN-only** application for distributing game files, sharing arbitrary files, and running a networked video "jukebox" at a LAN party among trusted friends. Cross-platform (**Windows + macOS**; Linux not a supported target). Personal/hobby scale: ~20 clients, ~500 GB of game files, no internet except YouTube/streaming playback and manual update checks.

**Naming:** product "Buttz LAN Tool", identifier `BLT` everywhere machine-facing. Binaries `blt-server` and `blt`; macOS bundle id `com.buttz.blt`; mDNS service `_blt._tcp`; metadata sidecar `.blt/`. Primary dev OS is **macOS**; Windows is the cross-build/CI target.

## Source of truth

The design is fully specified in three documents. **Read them before implementing. They override anything you'd otherwise assume.**

- `docs/01_PRODUCT_PLAN.md` — vision, personas, scope, milestones M0–M8, risks.
- `docs/02_FEATURE_SPEC.md` — features F0–F14, each with numbered, testable acceptance criteria (AC). **The AC are your definition of done.**
- `docs/03_TECHNICAL_DESIGN.md` — architecture, repo layout, SQLite schemas, manifest/chunk format, protocol, P2P, jukebox state machine, and the sequenced implementation plan (§12).

If a requirement is ambiguous, prefer the most specific statement across these docs. If genuinely undefined, pick the simplest choice consistent with the design principles and **leave a `// DESIGN-NOTE:` comment** explaining the assumption so it can be reviewed.

## Stack (do not substitute without asking)

- **Desktop app:** Tauri 2 — Rust backend (`apps/desktop/src-tauri`) + React/TypeScript/Vite UI (`apps/desktop/src`). One binary, runs in **client** or **playback** mode.
- **Server:** standalone Rust binary, **Axum** + **tokio**, system tray, serves a password-gated web admin panel.
- **Shared logic:** a Rust **`core`** crate (`crates/core`) consumed by both server and desktop app — manifest, chunking, hashing, protocol types, transfer, P2P, discovery.
- **Persistence:** SQLite (`rusqlite` preferred for simplicity; `sqlx` acceptable) on both server and client.
- **Hashing:** BLAKE3. **Discovery:** mDNS (`mdns-sd`). **Admin panel SPA:** `admin-web/`.
- See `docs/03_TECHNICAL_DESIGN.md` §1–§2 for the full table and repo layout. Follow that layout.

## Repo layout

Create and follow exactly the layout in `docs/03_TECHNICAL_DESIGN.md` §2 (Cargo workspace with `crates/core`, `crates/server`, `apps/desktop`, `admin-web`, `docs/`, `.github/workflows/`).

## How to work — milestone by milestone

Implement in milestone order **M0 → M8** as defined in `docs/03_TECHNICAL_DESIGN.md` §12. Each milestone maps to feature ACs in `docs/02_FEATURE_SPEC.md`.

For each milestone:
1. State which milestone and which F-ACs you're implementing.
2. If the milestone is large (notably **M4 P2P**, **M6 jukebox**), first break it into ordered, individually-testable steps, then implement them.
3. Implement, keeping `core` changes cohesive and separate from server/client wiring where practical.
4. Write tests that encode the milestone's acceptance criteria (see Testing).
5. Run build + tests; fix until green.
6. **Commit locally** with a message naming the milestone and ACs satisfied (e.g. `M2: server-direct download + resume (F4.5–F4.9, F5.1)`).
7. Update `PROGRESS.md` (see below), then proceed to the next milestone.

Do not skip ahead past a milestone's dependencies (the §12 table lists them). M0's scaffolding + test harness must exist before feature work.

## Testing

- Establish the test harness in **M0** so ACs become runnable assertions as you go.
- Rust: unit tests in-crate; integration tests under `tests/`. Prefer testing `core` logic (hashing, chunking, manifest diff, bitmap resume, rate-cap token bucket, throughput EWMA, jukebox ordering) directly and thoroughly — it's the highest-value, most testable surface.
- For transfer/resume/P2P, write integration tests that simulate interruption (drop a connection mid-transfer and assert resume from the bitmap) and corruption (feed a bad chunk and assert rejection + refetch).
- UI/end-to-end can be lighter; focus automated tests on the core and server logic. Note any AC that can only be validated manually in `PROGRESS.md`.

## Build / run commands

> Fill these in as you scaffold M0; keep this section accurate.

- Workspace build: `cargo build`
- Tests: `cargo test`
- Lint/format: `cargo fmt --all` and `cargo clippy --all-targets -- -D warnings`
- Desktop app dev: `cd apps/desktop && npm install && npm run tauri dev`
- Server run: `cargo run -p server`

## Conventions

- **One feature/concern per commit.** Keep commits coherent and milestone-tagged.
- **Isolate `core` changes** so server and desktop pick them up cleanly.
- Rust: `cargo fmt` + `clippy` clean (treat warnings as errors). No `unwrap()`/`expect()` on fallible paths that can occur at runtime — handle errors and surface them to the UI/log.
- TypeScript: keep dependencies minimal; no heavy state libraries unless justified.
- **No `localStorage`/`sessionStorage`** reliance for durable state — use SQLite via the Rust side.
- Keep functions small and the module boundaries in §2 intact.

## HARD CONSTRAINTS — never violate these

These come from the design and are non-negotiable. If a task seems to require breaking one, **stop and flag it** rather than proceeding.

1. **No chunk is ever written to disk unverified.** Every chunk (from server or peer) is BLAKE3-checked against the manifest before being accepted/written. A bad peer must never be able to corrupt a title.
2. **Manual updates only.** The self-updater shows "update available" + a "Download & restart" button. **Never** auto-download, auto-install, or auto-restart. Especially never restart a locked playback client unprompted.
3. **No DRM circumvention.** Netflix/Hulu/Prime are handled only by launching the real installed browser/app externally (the external-launch lane). Do **not** attempt to embed Widevine, spoof a CDM, or bypass content protection in any way.
4. **Trusted-LAN posture, but integrity always enforced.** No auth on data services (by design), but integrity via manifest hashes is mandatory. The admin panel and playback-lockdown enter/exit are password-gated; that gate must hold.
5. **Confirmation gates** for destructive/expensive actions: uploads, deletes, "download all", and external launches require explicit user confirmation in the initiating UI.
6. **Free-space pre-flight** on game and share downloads: warn + confirm when the destination volume lacks space (warn, don't hard-block).
7. **Airgap-friendly:** everything except YouTube/streaming playback and the manual update check must work with no internet.
8. **Dedicated playback client never downloads games** and is playback-only when locked.
9. **Games are stored UNZIPPED** as full folder trees on the server — never zip/archive titles. The unzipped layout is what enables per-file/per-chunk manifests, resume, diff, and P2P. Do not add an extraction step.
10. **Two decoupled payloads per title:** a structural **manifest** (files/chunks/hashes, the hot-path index) and a separate **title-info payload** (metadata + base64 cover, hashed via `info_hash`, client-cached). A cover/info edit must not bump the manifest version, and the `.blt/` sidecar must be excluded from the distributable manifest. Metadata is optional with graceful fallback (folder name; placeholder cover).
11. **Sanitize all received shared-pool path names** on the writing side (path-traversal, OS-illegal chars, length, case collisions) — a folder authored on one OS must write safely on another. Never write an untrusted relative path verbatim.
12. **The `/ws` live channel must auto-reconnect + resync** state on drop; consumers (jukebox, progress, roster) must tolerate disconnection and never present stale state as current.
13. **Votes key on `client_id`, not display name.** Server SQLite runs in **WAL mode** with a periodic backup snapshot.
14. **App binary and data are separate.** Binary installs to `Program Files\Buttz LAN Tool\` (Win, NSIS) / `/Applications/Buttz LAN Tool.app` (Mac, DMG, un-notarized → right-click-Open first launch). All runtime data goes in a single configurable **data root** (default `%LOCALAPPDATA%\BLT\` / `~/Library/Application Support/BLT/`), with `server/` and `client/` subfolders. The updater replaces only the binary and must NEVER touch the data root. The server's game-library/staging/share paths are configured separately (big drives) and are NOT under the data root. See TDD §17.
15. **Post-install scripts** are Windows-only, run **client-side after download+validation**, **only for canonical-library titles (never the shared pool)**, and must be **shown to the user and confirmed (with contents viewable) before running**. Never auto-run server-provided code. Launch options come from `info.json`'s `launch` block; no block → no Play button. BLT distributes and optionally launches — it is not a full launcher.
16. **Log via `tracing` to rotating files** (server + client separate, under each data subfolder's `logs/`). Client views its own log; admin panel views the server log. Never log secrets.

## Progress tracking

Maintain a `PROGRESS.md` at the repo root. After each milestone, record: milestone completed, which ACs are satisfied and tested, which ACs are deferred or only manually-verifiable, any `DESIGN-NOTE` assumptions made, and what's next. This is how the human picks up review.

## What "done with the first pass" means

A locally-runnable build where the core flows work end to end and the acceptance criteria are largely implemented and tested, ready for a human to validate. See the kickoff prompt for the exact target. When you reach it, summarize status in `PROGRESS.md` and list what needs manual validation.
