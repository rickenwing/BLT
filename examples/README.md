# Authoring a title's `.blt/info.json` (server side)

Each canonical-library game folder may contain an optional `.blt/` sidecar that
gives the client richer metadata than the bare folder name. **Everything here
is optional** — a folder with no `.blt/` still publishes fine (it shows by
folder name with a placeholder cover).

Layout inside a game folder:

```
My Game/
  .blt/
    info.json        <- metadata (this file)
    cover.png        <- optional cover image (png/jpg/jpeg/webp/gif)
    install.ps1      <- optional Windows post-install script (if referenced)
  Game.exe
  ...game files...
```

The `.blt/` folder is the admin's **input** format only; it is excluded from
the distributable manifest and never shipped to clients verbatim. Editing only
the sidecar bumps `info_hash` (cover/metadata) without bumping the manifest
version, so a metadata tweak does **not** force clients to re-download the game.

See `sample-title/.blt/info.json` for a ready-to-copy template.

## Fields (all optional)

| Key | Type | Notes |
|---|---|---|
| `name` | string | **The displayed title name.** Omitted → folder name is used. |
| `year` | number | Release year, e.g. `2026`. |
| `genre` | string | Free text. |
| `players` | string | Free text, e.g. `"1-8 local"`. |
| `blurb` | string | Short description on the title card. |
| `link` | string | Optional homepage URL. |
| `launch` | array | Launch entries; **no entries → no Play button.** |
| `install_script` | object | `{ "windows": "install.ps1" }` — Windows-only, path relative to the game folder. Shown to the user and confirmed before it runs. |

`launch[]` entries: `name` and `exe` are required; `args` and `cwd` are
optional (`cwd` defaults to the game folder).

## Common mistakes

- **Use `name`, not `label`, for the title.** `label` is ignored (unknown keys
  are silently dropped); the displayed name comes from `name`. (`titles.label`
  in the server DB is a separate admin-panel override, unrelated to info.json.)
- **There is no `version` field.** The manifest version is assigned
  automatically by the scanner; putting `version` in info.json does nothing.
- **The cover is a separate file, not inline.** Drop `cover.png` (or
  jpg/jpeg/webp/gif) in `.blt/`. There is no `cover_b64` input key — the server
  embeds the image itself when serving.
- Malformed `info.json` does not block the game from publishing; the server
  logs a warning and falls back to folder name + placeholder cover.
