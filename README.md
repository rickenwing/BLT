# Buttz LAN Tool (BLT)

Self-hosted, LAN-only game distribution + shared file pool + networked video
jukebox for a LAN party among trusted friends. Windows + macOS.

- **`blt-server`** — headless server (Axum): game library, shared pool, jukebox
  state, password-gated web admin panel. Runs on the dedicated multi-NIC box.
- **`Buttz LAN Tool.app` / `.exe`** — Tauri desktop app; runs in **client**
  (players) or **playback** mode (the machine wired to the TV).

Design docs live in [`docs/`](docs/) — product plan, feature spec (the
acceptance criteria), and the technical design. Progress + deviations are
tracked in [`PROGRESS.md`](PROGRESS.md).

## Build from source

Prereqs: Rust (stable), Node 22+.

```sh
cargo test                                   # core + server suites (incl. integration)
cargo build --release -p blt-server          # the server binary

cd admin-web && npm install && npm run build # admin SPA (served by the server)
cd apps/desktop && npm install && npm run tauri build   # desktop bundles (DMG/NSIS)
```

Dev loops: `cargo run -p blt-server` · `cd admin-web && npm run dev` (proxies
to `127.0.0.1:7402`) · `cd apps/desktop && npm run tauri dev`.

## Server setup (the host)

1. Run `blt-server` (data lands in `%LOCALAPPDATA%\BLT\server` /
   `~/Library/Application Support/BLT/server`; override with `--data-root`).
2. Open the admin panel — `http://<server>:7402` — and set the admin password
   (first run), then under **Settings**:
   - **Library path**: each subfolder = one game, stored **unzipped**.
   - **Staging path**: same volume as the library; copy new games here — they
     auto-promote once files stop changing (~30 s).
   - **Share path**: the shared-pool drive.
   - Bind each service to the NIC you want (game / share / admin can split
     across up to three).
3. **Scan now** (or wait for the auto-scan). Published titles appear in
   clients immediately.
4. Optional per-title metadata: a `.blt/` folder inside the game with
   `info.json` (name, year, genre, players, blurb, `launch` entries, Windows
   `install_script`) and a `cover.png/jpg`. Editing it never re-versions the
   game files.

Locked out of the admin panel by a bad bind? Edit `config.toml` in the server
data folder and restart, or run `blt-server --reset-admin-bind`.

## Client setup (players)

First launch: pick **Player**, set your display name (defaults to the computer
name) and a download folder. The server is discovered via mDNS; if discovery
is blocked, type the game-service `ip:port` manually under Settings →
Connection.

Downloads are chunked, BLAKE3-verified, resumable across Wi-Fi drops, and
peer-accelerated (you seed what you've downloaded at a capped rate — toggle
and cap under Settings).

## Playback machine (the TV box)

First launch: pick **Playback machine** (or lock an existing client via
Settings → Playback lockdown — requires the admin password, as does exiting).
It plays the jukebox queue: YouTube and shared/direct videos embedded with
auto-advance; Netflix/Hulu/Prime items open in the real browser and wait for
a human to press **Next**.

> Log into the streaming services in that browser **before** the party.

## Pre-party network checklist (TDD §16)

- Allow the apps through the OS firewall on first launch (Windows: set the
  network profile to **Private**). If denied, discovery and transfers fail
  silently.
- Confirm AP/client isolation is **off** — have two laptops ping each other.
  If isolation is on, clients fall back to server-only downloads (visible in
  the People roster), which still works.
- Prefer copy tools that preserve timestamps when updating the library
  (change detection uses size+mtime).

## Releases & updates

Tagged releases build signed bundles via CI (`.github/workflows/release.yml`);
the Ed25519 private key lives only in CI secrets. Updates are **manual** —
the app indicates an update and the user chooses "Download & restart"; nothing
ever auto-installs (and a locked playback box is never restarted mid-party).

macOS bundles are un-notarized in v1: first launch is **right-click → Open**.
