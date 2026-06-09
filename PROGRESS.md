# BLT — Progress

> Maintained per CLAUDE.md. Records milestone status, AC coverage, design
> notes, and what a human should review/validate next.

## Status snapshot (2026-06-09, end of first pass)

| Area | State |
|---|---|
| `crates/core` | **Done** — 80+ unit tests, clippy-clean. |
| `crates/server` | **Done** — 17 unit + 10 integration tests; smoke-tested live (scan→publish→serve, SPA, auth). |
| `admin-web` SPA | **Done** — builds; served live by the server; all panel features. |
| `apps/desktop` (Tauri) | **Built** — Rust engine + full client/playback UI compile clean (clippy `-D warnings`, strict TS). GUI flows need manual validation. |
| CI / signing / release | **Workflows written** — CI (fmt/clippy/test, both OSes) + tagged signed releases via tauri-action. Unexercised until pushed to GitHub. |
| Self-update client wiring | **Done** — updater plugin wired to `github.com/rickenwing/BLT` releases; pubkey embedded; launch-time indicator + manual "Download & restart" in Settings; locked playback clients refuse in-app updates (F0.4). Remaining: add the two signing secrets to the GitHub repo, then tag. |
| Tray icon (server) | Deferred (documented); server runs headless. |

`cargo test --workspace` → 111 passing. `cargo clippy --workspace --all-targets -- -D warnings` → clean. `admin-web` + `apps/desktop` → strict-TS Vite builds clean.

## Milestones

### M0 — Skeleton & repo *(partially complete)*
- ✅ Cargo workspace (`crates/core`, `crates/server`); desktop + admin-web pending.
- ✅ Data-root resolution (per-OS default + `--data-root` override) with
  `BLT/server` + `BLT/client` subtrees (TDD §17) — `core::runtime::data_root`, tested.
- ✅ `tracing` rotating-file logging foundation (daily rotation, 14 files,
  `BLT_LOG` env filter) — `core::runtime::logging`.
- ⬜ CI build (Win+Mac), signing keys, release workflow — **next**.
- ⬜ Tauri app stub + mode switch — **next**.

### M1 — Manifests & library *(server side complete, admin SPA pending)*
- ✅ F2.1–F2.8: scan → manifest/chunks in SQLite; staging + settle promotion
  (atomic same-volume rename, replace-aside for updates); sidecar metadata
  (`info.json` + cover → info payload + `info_hash`); periodic + manual scan;
  removed-title detection (and re-publish when a folder reappears).
- ✅ F1.1–F1.5: three independent binds, hot rebind of a single listener,
  interface enumeration, `config.toml` recovery + `--reset-admin-bind`.
- Tested: `scan_publishes_manifest_excluding_sidecar`,
  `rescan_bumps_version_sidecar_change_does_not` (manifest_ver vs info_hash
  decoupling, HARD CONSTRAINT #10), `staging_promotes_only_when_stable`.

### M2 — Server distribution *(server side complete, client pending)*
- ✅ `/titles`, `/titles/{id}/manifest|info|script`, `/chunks/{f}/{i}`,
  Range-capable streaming `/titles/{id}/files/{f}`.
- ✅ Verified-chunk download loop + bitmap resume + quick validation exercised
  end-to-end in `chunked_download_resume_and_validate` (incl. tamper rejection
  — HARD CONSTRAINT #1) — the **client app** that drives this for real users is
  the next deliverable.

### M3–M7 *(server-side pieces in place, client/UI pending)*
- mDNS advertisement (IP:port TXT, #9) done server-side; client browse pending.
- Deep verify + repair plan + `finalize_layout` in core (tested).
- P2P primitives in core (token bucket, EWMA, weighted scheduler w/ bootstrap);
  WS peer registry + reachability probe handout + roster server-side (tested);
  client chunk server/scheduler wiring pending (M4).
- Shared pool server-complete: upload (sanitised, #11), listing, Range
  download/stream, owner/admin delete, delete-race 410 (tested). Client UI +
  drag-drop pending.
- Jukebox server-complete: queue/votes (client_id-keyed, #13), Fair Rotation +
  Vote-Ranked, pinned now-playing, external awaiting-human lane, admin
  REST + client WS add/vote, playback-only Next/Ended (tested over real WS).
- Admin auth: argon2 + session cookie, first-run setup flow (tested).

### M8 — pending
Log viewers (API exists server-side: `/api/log`), self-update wiring,
installers, first-run mode pick, settings polish.

## Multi-agent review (mid-build) — outcome

A 45-agent review validated plan adherence and hunted bugs; 7 confirmed
findings, all fixed with regression tests:
1. `repair_plan`/`validate_deep` couldn't converge on over-long files → added
   `core::transfer::finalize_layout` (truncate + materialise).
2. Zero-byte files never created by the chunk loop → same fix, tested.
3. P2P scheduler bootstrap deadlock (unmeasured peers permanently below the
   floor) → `throughput_bps: Option<f64>` + bootstrap weight.
4. TokenBucket livelock (burst 1.5 MB < 4 MiB chunk; finite `time_until` lie)
   → burst ≥ one chunk, `time_until` returns ∞ for impossible requests.
5. Lock-poisoning zombie risk (`.expect` on every lock) → `parking_lot`.
6. Jukebox error-swallowing + contradictory `now_playing+Idle` state → fixed.
7. Corrupt hash blobs silently became zero-hashes → hard error; cache reload
   skips + logs the corrupt title only.

Plus triaged from the unverified batch: `ServerMsg::Roster` failed tagged-enum
serialisation at runtime (found via integration test, now has an
every-variant round-trip test); streaming for whole-file serving (no multi-GB
RAM buffering); playback-mode sessions refused from the peer registry (#8);
case-insensitive `.blt` sidecar exclusion; `:`-component rejection in
`safe_join`/script paths (Windows drive-relative + NTFS ADS); Config Debug
redaction (#16); fair-rotation strict round-robin; single-step DB backup.

## DESIGN-NOTEs (deviations / assumptions to review)

- `titles` table stores the served info payload as one `info_json` TEXT column
  instead of the TDD's discrete `meta_*` columns + `cover_b64`. Functionally
  equivalent; serves `/titles/{id}/info` directly. (db.rs)
- `jukebox_items.sort_override` added (not in TDD schema) to reconcile F11.1
  admin reorder with dynamic ordering modes. (db.rs)
- Admin SPA served from disk (`BLT_ADMIN_WEB` or next to binary); embedding
  (rust-embed) is a drop-in M8 change. (admin_api.rs)
- Staging promotion of an *existing* title moves the old tree aside then
  renames (TDD accepts the rare mid-update window). (library.rs)
- Admin panel binds `0.0.0.0:7402` by default (persona reaches it from their
  laptop; password-gated). TDD lists no explicit default.
- Tray icon deferred to M8 ("tray UX" is scheduled there); server runs
  headless meanwhile.
- Single-file shares flatten to basename; multi-file uploads are sent one
  share per POST by our clients. (share.rs)

## Manual-only validation list (so far)

- mDNS advertisement visibility on a real LAN (unit-testable parts are tested;
  the daemon needs a network).
- Multi-NIC bind split (F1.2) on a real 3-NIC host.
- Staging settle window with a real slow copy (logic tested with settle=0).
- Firewall prompts / first-launch UX (TDD §16).

## Milestone coverage after the desktop app

- **M0** ✅ (tray deferred, documented) — workspace, data-root, logging, CI,
  signing/release workflows, Tauri stub w/ first-run mode pick.
- **M1** ✅ — scan/manifest/staging/binds + admin panel lists/renames titles.
- **M2** ✅ — server-direct chunked download w/ pause/resume/retry, sequential
  visible queue, per-title + default paths, quick validation, client bitmap.
- **M3** ✅ — mDNS advertise (server) + browse (client) w/ manual fallback;
  deep verify + repair; download-all vs selective; cover/info display cached
  by info_hash.
- **M4** ✅ core paths — peer registry/WS, client chunk server (capped),
  verify-from-peers, share-back toggle, reachability self-test w/ visible
  server-only fallback, EWMA throughput, weighted scheduler, roster.
  *Refinement noted:* peers advertise whole titles (no per-chunk have-maps in
  v1 — failures blacklist a peer and the server fills in).
- **M5** ✅ — shared pool file+folder upload (drag-drop + pickers, confirm
  gates), structure-preserving sanitised downloads, X-of-N completeness +
  re-fetch missing, owner/admin delete rules, display names, post-install
  scripts (view+confirm, hash-pinned, Windows-only) + launch options.
- **M6** ✅ — queue+upvote (client_id-keyed), Fair Rotation/Vote-Ranked,
  embedded YouTube (IFrame API, ended→advance) + HTML5 LAN streaming, client
  add/vote view, admin queue controls.
- **M7** ✅ — external/DRM lane (real browser, awaiting-human, Next from
  playback or admin), playback lockdown w/ server-verified admin password.
- **M8** ◐ — admin password login ✅, lockdown gate ✅, log viewers (client +
  admin/server) ✅, first-run mode pick ✅, `--reset-admin-bind` ✅, NSIS/DMG
  bundling via CI ✅ (unexercised), docs/README ✅. **Remaining:** in-app
  update indicator (needs repo URL + pubkey — see snapshot), server tray UX,
  settings polish from real-world use.

## Manual validation needed (GUI flows; the engine beneath them is tested)

1. `npm run tauri dev` — first-run mode pick, connect to a live server,
   browse covers, download w/ pause/resume mid-transfer, validate, Play.
2. Drag-drop a folder into Shares from Finder/Explorer; download it on a
   second client; delete-race behaviour.
3. Two clients + playback machine: queue YouTube + a shared video + a Netflix
   link; auto-advance; external await + Next; lockdown enter/exit.
4. Real-LAN checks: mDNS discovery, AP-isolation → server-only roster badge,
   3-NIC bind split, Windows build (`cargo build` + NSIS via CI).
