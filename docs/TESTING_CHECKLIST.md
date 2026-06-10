# BLT v0.1.0 — Manual validation checklist

Three-machine LAN test. **Server** = macOS dev machine (also runs a desktop
client in client mode so P2P has two downloaders), **Client** = Windows
machine, **Playback** = third machine in playback mode.

> **WiFi-only is fine.** All services bind the single WiFi NIC. Two caveats
> that double as test inputs: (1) router **AP isolation** blocks client↔client
> traffic — mDNS and P2P will fail and the reachability self-test must show
> the server-only fallback badge; (2) multicast over WiFi is flaky on some
> APs, so mDNS may fail even without isolation — manual IP entry is the
> designed fallback. The multi-NIC bind split (F1.2) is untestable here and
> remains on the needs-special-hardware list.

Record results per item in `PROGRESS.md` ("Manual validation needed" section).

## 0 — Setup

- [ ] From the v0.1.0 release: NSIS installer on Windows, DMG on the Mac(s)
      (un-notarized → right-click → Open on first launch); unpack
      `blt-server-macos.tar.gz` here
- [ ] Note both OS firewall prompts on first launch; record whether any manual
      firewall config was needed (TDD §16)
- [ ] Prepare a library folder (NOT under the data root) with 2–3 small game
      folders; give one a `.blt/` sidecar (`info.json` with metadata + `launch`
      block + `install_script.windows`, plus a `cover.*` image) and leave one
      bare (folder-name + placeholder-cover fallback path)
- [ ] Start `blt-server`; first-run admin password setup; data root appears at
      `~/Library/Application Support/BLT/server/`

## 1 — Server + admin panel (F1, F2, F11, F17)

- [ ] Open the admin panel from the **Windows** machine (not localhost); log in
- [ ] Scan publishes the titles; bare folder shows folder name + placeholder
      cover
- [ ] Edit the sidecar cover/info on disk, rescan → cover/info updates on
      clients **without** the title re-downloading (info_hash bumps,
      manifest_ver doesn't — HARD CONSTRAINT #10)
- [ ] Copy a new title into staging slowly (large file mid-copy) → not
      published until stable; then promotes
- [ ] Remove a title folder, rescan → title disappears; restore → re-publishes
- [ ] Admin log viewer shows the live server log; spot-check no secrets in it

## 2 — Discovery & connect (F3)

- [ ] Windows client: first-run mode pick → client; server appears via mDNS —
      if not (AP/multicast), manual IP works and is remembered
- [ ] Restart the server while clients are open → clients show disconnected
      (never stale-as-current), auto-reconnect and resync (#12)

## 3 — Downloads & validation (F4, F5, F15)

- [ ] Browse covers; set default download path; per-title override
- [ ] Free-space pre-flight: download to a nearly-full destination → warn +
      confirm, not a hard block (#6)
- [ ] Download a title; pause/resume mid-transfer; kill the client
      mid-download, relaunch → resumes from the bitmap, not from zero
- [ ] Quick validate after completion; corrupt one file on disk → deep verify
      finds it, repair re-fetches only the bad piece
- [ ] "Download all" requires confirmation (#5)
- [ ] Title with `launch` block shows Play and launches; bare title has no
      Play button
- [ ] Windows, sidecar title: post-install script is shown with viewable
      contents and requires confirm before running (#15)

## 4 — P2P & roster (F4 M4, F13) — needs the Mac client running

- [ ] Mac client fully downloads a title with share-back on; Windows client
      downloads the same title → transfer view shows chunks from the peer,
      not only the server
- [ ] Toggle share-back off on the Mac client → Windows falls back to
      server-only
- [ ] Reachability self-test matches router reality (peer-reachable, or
      AP-isolated → visible server-only badge)
- [ ] Roster throughput numbers look sane for WiFi

## 5 — Shared pool (F6, F7)

- [ ] Set display names on both clients; they show in roster/shares
- [ ] Drag-drop a folder from Finder into Shares (confirm gate) — include a
      file with Windows-illegal characters (e.g. `notes: draft?.txt`) →
      Windows client writes a sanitized, safe name (#11)
- [ ] Drag-drop from Explorer on Windows → download on the Mac
- [ ] X-of-N completeness shown; delete a local file → re-fetch missing works
- [ ] Owner can delete own share; admin can delete any; non-owner cannot
- [ ] Delete race: start a download, delete the share from the other machine
      mid-stream → clean error, no crash or partial junk

## 6 — Jukebox (F8–F11)

- [ ] Playback machine: mode pick → playback; confirm it **cannot** download
      games (#8)
- [ ] Queue a YouTube link from each client; upvote from both → Vote-Ranked
      reorders; Fair Rotation → strict round-robin by submitter
- [ ] Votes survive a display-name change (keyed on client_id, #13)
- [ ] Queue a video from the shared pool → LAN streaming playback;
      auto-advance on ended
- [ ] Queue a Netflix/DRM link → external lane: "awaiting human" on the
      playback machine, real browser launches (after confirm), Next works from
      both the playback machine and the admin panel
- [ ] Admin: reorder, remove, pin now-playing

## 7 — Lockdown & update (F14, F0.4)

- [ ] Lock the playback client (admin password); UI is playback-only; exit
      requires the password; wrong password rejected
- [ ] While locked: in-app update refused (#2)
- [ ] Settings → check for updates on v0.1.0 → "up to date" (requires the
      release to be **published**, not draft; the full update flow needs a
      future v0.1.1)

## 8 — Airgap (#7) — do last

- [ ] Kill internet at the router (LAN stays up): everything above except
      YouTube/external playback and the update check still works
