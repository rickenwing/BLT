//! Server SQLite (TDD §4.1). WAL mode + a periodic backup snapshot is set up in
//! [`open`] and `main` (HARD CONSTRAINT #13 / F15.7). Helpers take a
//! `&Connection`; the connection itself is held behind a `Mutex` in `AppState`
//! and never locked across an `.await`.

use blt_core::hashing::Hash;
use blt_core::manifest::{ChunkEntry, FileEntry, Manifest};
use blt_core::protocol::{ShareKind, ShareSummary};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;

/// Open (or create) the database in WAL mode and run migrations.
pub fn open(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&conn)?;
    Ok(conn)
}

/// In-memory DB for tests.
#[cfg(test)]
pub fn open_memory() -> anyhow::Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS config (key TEXT PRIMARY KEY, value TEXT NOT NULL);

        CREATE TABLE IF NOT EXISTS titles (
          id            INTEGER PRIMARY KEY,
          name          TEXT NOT NULL UNIQUE,
          label         TEXT,
          manifest_ver  INTEGER NOT NULL,
          total_size    INTEGER NOT NULL,
          file_count    INTEGER NOT NULL,
          state         TEXT NOT NULL,
          last_scan     INTEGER NOT NULL,
          info_hash     BLOB,
          info_json     TEXT,
          has_cover     INTEGER NOT NULL DEFAULT 0,
          has_install_script INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS files (
          id        INTEGER PRIMARY KEY,
          title_id  INTEGER NOT NULL REFERENCES titles(id) ON DELETE CASCADE,
          rel_path  TEXT NOT NULL,
          size      INTEGER NOT NULL,
          mtime     INTEGER NOT NULL,
          hash      BLOB NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_files_title ON files(title_id);

        CREATE TABLE IF NOT EXISTS chunks (
          id        INTEGER PRIMARY KEY,
          file_id   INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
          idx       INTEGER NOT NULL,
          offset    INTEGER NOT NULL,
          size      INTEGER NOT NULL,
          hash      BLOB NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_chunks_file ON chunks(file_id);

        CREATE TABLE IF NOT EXISTS shares (
          id           INTEGER PRIMARY KEY,
          name         TEXT NOT NULL,
          kind         TEXT NOT NULL,
          size         INTEGER NOT NULL,
          file_count   INTEGER NOT NULL,
          owner_name   TEXT NOT NULL,
          owner_client TEXT,
          stored_path  TEXT NOT NULL,
          created_at   INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS share_files (
          id        INTEGER PRIMARY KEY,
          share_id  INTEGER NOT NULL REFERENCES shares(id) ON DELETE CASCADE,
          rel_path  TEXT NOT NULL,
          size      INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_share_files_share ON share_files(share_id);

        -- DESIGN-NOTE: sort_override is not in the TDD schema. F11.1 gives the
        -- admin "reorder" but both ordering modes recompute dynamically; an
        -- explicit override rank (NULL = follow the mode) reconciles the two:
        -- overridden items sort first (ascending), the rest follow the mode.
        CREATE TABLE IF NOT EXISTS jukebox_items (
          id            INTEGER PRIMARY KEY,
          type          TEXT NOT NULL,
          ref           TEXT NOT NULL,
          title         TEXT,
          added_by      TEXT NOT NULL,
          added_by_id   TEXT NOT NULL,
          added_at      INTEGER NOT NULL,
          state         TEXT NOT NULL,
          sort_override INTEGER
        );

        CREATE TABLE IF NOT EXISTS jukebox_votes (
          item_id   INTEGER NOT NULL REFERENCES jukebox_items(id) ON DELETE CASCADE,
          voter_id  TEXT NOT NULL,
          PRIMARY KEY (item_id, voter_id)
        );

        CREATE TABLE IF NOT EXISTS names (
          client_id  TEXT PRIMARY KEY,
          name       TEXT NOT NULL,
          updated_at INTEGER NOT NULL
        );
        "#,
    )?;
    Ok(())
}

// ───────────────────────────── config kv ───────────────────────────

pub fn set_config(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO config(key,value) VALUES(?1,?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn get_config(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM config WHERE key=?1", params![key], |r| {
        r.get::<_, String>(0)
    })
    .optional()
}

// ───────────────────────────── titles ──────────────────────────────

/// A published title's persisted fields needed for listing.
#[derive(Debug, Clone)]
pub struct TitleRow {
    pub id: u64,
    pub name: String,
    pub label: Option<String>,
    pub manifest_ver: u32,
    pub total_size: u64,
    pub file_count: u64,
    pub state: String,
    pub last_scan: i64,
    pub info_hash: Option<Hash>,
    pub has_cover: bool,
    pub has_install_script: bool,
}

fn row_to_title(r: &rusqlite::Row) -> rusqlite::Result<TitleRow> {
    let info_hash: Option<Vec<u8>> = r.get("info_hash")?;
    Ok(TitleRow {
        id: r.get::<_, i64>("id")? as u64,
        name: r.get("name")?,
        label: r.get("label")?,
        manifest_ver: r.get::<_, i64>("manifest_ver")? as u32,
        total_size: r.get::<_, i64>("total_size")? as u64,
        file_count: r.get::<_, i64>("file_count")? as u64,
        state: r.get("state")?,
        last_scan: r.get("last_scan")?,
        info_hash: info_hash.and_then(|b| Hash::from_slice(&b).ok()),
        has_cover: r.get::<_, i64>("has_cover")? != 0,
        has_install_script: r.get::<_, i64>("has_install_script")? != 0,
    })
}

pub fn list_titles(conn: &Connection) -> rusqlite::Result<Vec<TitleRow>> {
    let mut stmt = conn.prepare(
        "SELECT id,name,label,manifest_ver,total_size,file_count,state,last_scan,
                info_hash,has_cover,has_install_script
         FROM titles WHERE state != 'removed' ORDER BY name",
    )?;
    let rows = stmt
        .query_map([], row_to_title)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_title(conn: &Connection, id: u64) -> rusqlite::Result<Option<TitleRow>> {
    conn.query_row(
        "SELECT id,name,label,manifest_ver,total_size,file_count,state,last_scan,
                info_hash,has_cover,has_install_script
         FROM titles WHERE id=?1",
        params![id as i64],
        row_to_title,
    )
    .optional()
}

pub fn title_id_by_name(conn: &Connection, name: &str) -> rusqlite::Result<Option<u64>> {
    conn.query_row("SELECT id FROM titles WHERE name=?1", params![name], |r| {
        r.get::<_, i64>(0).map(|v| v as u64)
    })
    .optional()
}

pub fn set_title_label(conn: &Connection, id: u64, label: Option<&str>) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE titles SET label=?2 WHERE id=?1",
        params![id as i64, label],
    )?;
    Ok(())
}

pub fn set_title_state(conn: &Connection, id: u64, state: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE titles SET state=?2 WHERE id=?1",
        params![id as i64, state],
    )?;
    Ok(())
}

pub fn get_info_json(conn: &Connection, id: u64) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT info_json FROM titles WHERE id=?1",
        params![id as i64],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    .map(Option::flatten)
}

/// Insert or fetch a title id by folder name, leaving metadata for the scan to fill.
pub fn ensure_title(conn: &Connection, name: &str) -> rusqlite::Result<u64> {
    if let Some(id) = title_id_by_name(conn, name)? {
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO titles(name,label,manifest_ver,total_size,file_count,state,last_scan)
         VALUES(?1,NULL,0,0,0,'scanning',0)",
        params![name],
    )?;
    Ok(conn.last_insert_rowid() as u64)
}

/// Replace a title's manifest (files + chunks) and bump its version + metadata.
/// File ids are kept **stable across re-scans** by upserting on
/// `(title_id, rel_path)`; only changed files get new chunk rows, and files no
/// longer present are removed. `FileEntry.id` on input is ignored. Runs in a
/// single transaction.
#[allow(clippy::too_many_arguments)]
pub fn replace_manifest(
    conn: &mut Connection,
    title_id: u64,
    files: &[FileEntry],
    total_size: u64,
    file_count: u64,
    new_version: u32,
    last_scan: i64,
    info_hash: Option<&Hash>,
    info_json: Option<&str>,
    has_cover: bool,
    has_install_script: bool,
) -> rusqlite::Result<()> {
    use std::collections::HashSet;
    let tx = conn.transaction()?;

    // Existing files by rel_path → (id, size, mtime, hash), to preserve ids and
    // skip rewriting files unchanged since the last scan.
    let mut existing: HashMap<String, (i64, u64, i64, Vec<u8>)> = HashMap::new();
    {
        let mut stmt =
            tx.prepare("SELECT id, rel_path, size, mtime, hash FROM files WHERE title_id=?1")?;
        let rows = stmt.query_map(params![title_id as i64], |r| {
            Ok((
                r.get::<_, String>(1)?,
                (
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Vec<u8>>(4)?,
                ),
            ))
        })?;
        for row in rows {
            let (rel, meta) = row?;
            existing.insert(rel, meta);
        }
    }

    let mut seen: HashSet<String> = HashSet::new();
    for f in files {
        seen.insert(f.rel_path.clone());
        let file_id = if let Some((id, size, mtime, hash)) = existing.get(&f.rel_path) {
            let (id, size, mtime) = (*id, *size, *mtime);
            // Unchanged (same size, mtime, content hash) → leave its row and chunks
            // as-is instead of UPDATE + DELETE + re-INSERT, which is the bulk of a
            // re-scan's write traffic for an unchanged title.
            if size == f.size && mtime == f.mtime && hash.as_slice() == f.hash.as_bytes().as_slice()
            {
                continue;
            }
            tx.execute(
                "UPDATE files SET size=?2, mtime=?3, hash=?4 WHERE id=?1",
                params![id, f.size as i64, f.mtime, f.hash.as_bytes().as_slice()],
            )?;
            tx.execute("DELETE FROM chunks WHERE file_id=?1", params![id])?;
            id
        } else {
            tx.execute(
                "INSERT INTO files(title_id,rel_path,size,mtime,hash) VALUES(?1,?2,?3,?4,?5)",
                params![
                    title_id as i64,
                    f.rel_path,
                    f.size as i64,
                    f.mtime,
                    f.hash.as_bytes().as_slice()
                ],
            )?;
            tx.last_insert_rowid()
        };
        for c in &f.chunks {
            tx.execute(
                "INSERT INTO chunks(file_id,idx,offset,size,hash) VALUES(?1,?2,?3,?4,?5)",
                params![
                    file_id,
                    c.idx as i64,
                    c.offset as i64,
                    c.size as i64,
                    c.hash.as_bytes().as_slice()
                ],
            )?;
        }
    }
    // Remove files no longer present (chunks cascade).
    for (rel, (id, _, _, _)) in &existing {
        if !seen.contains(rel) {
            tx.execute("DELETE FROM files WHERE id=?1", params![id])?;
        }
    }

    tx.execute(
        "UPDATE titles SET manifest_ver=?2, total_size=?3, file_count=?4, state='published',
                last_scan=?5, info_hash=?6, info_json=?7, has_cover=?8, has_install_script=?9
         WHERE id=?1",
        params![
            title_id as i64,
            new_version as i64,
            total_size as i64,
            file_count as i64,
            last_scan,
            info_hash.map(|h| h.as_bytes().to_vec()),
            info_json,
            has_cover as i64,
            has_install_script as i64,
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// Update only the info payload (cover/metadata) without bumping the manifest
/// version (HARD CONSTRAINT #10 / F2.6).
pub fn update_info_only(
    conn: &Connection,
    title_id: u64,
    info_hash: Option<&Hash>,
    info_json: Option<&str>,
    has_cover: bool,
    has_install_script: bool,
    last_scan: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE titles SET info_hash=?2, info_json=?3, has_cover=?4, has_install_script=?5, last_scan=?6
         WHERE id=?1",
        params![
            title_id as i64,
            info_hash.map(|h| h.as_bytes().to_vec()),
            info_json,
            has_cover as i64,
            has_install_script as i64,
            last_scan
        ],
    )?;
    Ok(())
}

/// Load a full manifest from the DB (used to populate the in-memory cache).
pub fn load_manifest(conn: &Connection, title_id: u64) -> rusqlite::Result<Option<Manifest>> {
    let title = match get_title(conn, title_id)? {
        Some(t) => t,
        None => return Ok(None),
    };
    let chunk_size: u64 = get_config(conn, "chunk_size")?
        .and_then(|s| s.parse().ok())
        .unwrap_or(blt_core::DEFAULT_CHUNK_SIZE);

    let mut fstmt = conn
        .prepare("SELECT id,rel_path,size,mtime,hash FROM files WHERE title_id=?1 ORDER BY id")?;
    let file_rows = fstmt
        .query_map(params![title_id as i64], |r| {
            let hash: Vec<u8> = r.get("hash")?;
            Ok((
                r.get::<_, i64>("id")? as u64,
                r.get::<_, String>("rel_path")?,
                r.get::<_, i64>("size")? as u64,
                r.get::<_, i64>("mtime")?,
                hash,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // A malformed hash blob (DB corruption / bad migration) is a hard error —
    // substituting a placeholder would serve a manifest whose chunks can never
    // verify, presenting as endless client-side "network" failures instead of
    // the server-side data problem it is.
    fn parse_hash(b: &[u8], what: &str) -> rusqlite::Result<Hash> {
        Hash::from_slice(b).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                b.len(),
                rusqlite::types::Type::Blob,
                format!("corrupt {what} hash blob ({} bytes, expected 32)", b.len()).into(),
            )
        })
    }

    // Prepare the chunk query once and rebind per file, rather than recompiling
    // it for every file in the title.
    let mut cstmt =
        conn.prepare("SELECT idx,offset,size,hash FROM chunks WHERE file_id=?1 ORDER BY idx")?;
    let mut files = Vec::with_capacity(file_rows.len());
    for (fid, rel_path, size, mtime, fhash) in file_rows {
        let chunks = cstmt
            .query_map(params![fid as i64], |r| {
                let h: Vec<u8> = r.get("hash")?;
                Ok(ChunkEntry {
                    idx: r.get::<_, i64>("idx")? as u32,
                    offset: r.get::<_, i64>("offset")? as u64,
                    size: r.get::<_, i64>("size")? as u64,
                    hash: parse_hash(&h, "chunk")?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        files.push(FileEntry {
            id: fid,
            rel_path,
            size,
            mtime,
            hash: parse_hash(&fhash, "file")?,
            chunks,
        });
    }

    Ok(Some(Manifest {
        title_id,
        manifest_ver: title.manifest_ver,
        total_size: title.total_size,
        file_count: title.file_count,
        chunk_size,
        files,
    }))
}

/// Locate a file on disk for the server seed path: returns
/// `(title_folder_name, rel_path, size)`. The absolute path is
/// `library_path / title_folder_name / rel_path`.
pub fn file_location(
    conn: &Connection,
    file_id: u64,
) -> rusqlite::Result<Option<(String, String, u64)>> {
    conn.query_row(
        "SELECT t.name, f.rel_path, f.size
         FROM files f JOIN titles t ON t.id = f.title_id
         WHERE f.id = ?1",
        params![file_id as i64],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
            ))
        },
    )
    .optional()
}

/// Locate one chunk: `(title_folder_name, rel_path, offset, size)`.
pub fn chunk_location(
    conn: &Connection,
    file_id: u64,
    idx: u32,
) -> rusqlite::Result<Option<(String, String, u64, u64)>> {
    conn.query_row(
        "SELECT t.name, f.rel_path, c.offset, c.size
         FROM chunks c JOIN files f ON f.id = c.file_id JOIN titles t ON t.id = f.title_id
         WHERE c.file_id = ?1 AND c.idx = ?2",
        params![file_id as i64, idx as i64],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, i64>(3)? as u64,
            ))
        },
    )
    .optional()
}

// ───────────────────────────── shares ──────────────────────────────

#[allow(clippy::too_many_arguments)]
pub fn insert_share(
    conn: &mut Connection,
    name: &str,
    kind: ShareKind,
    size: u64,
    owner_name: &str,
    owner_client: Option<&str>,
    stored_path: &str,
    created_at: i64,
    files: &[(String, u64)],
) -> rusqlite::Result<u64> {
    let kind_str = match kind {
        ShareKind::File => "file",
        ShareKind::Folder => "folder",
    };
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO shares(name,kind,size,file_count,owner_name,owner_client,stored_path,created_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            name,
            kind_str,
            size as i64,
            files.len() as i64,
            owner_name,
            owner_client,
            stored_path,
            created_at
        ],
    )?;
    let id = tx.last_insert_rowid() as u64;
    for (rel, sz) in files {
        tx.execute(
            "INSERT INTO share_files(share_id,rel_path,size) VALUES(?1,?2,?3)",
            params![id as i64, rel, *sz as i64],
        )?;
    }
    tx.commit()?;
    Ok(id)
}

fn row_to_share(r: &rusqlite::Row) -> rusqlite::Result<ShareSummary> {
    let kind = match r.get::<_, String>("kind")?.as_str() {
        "folder" => ShareKind::Folder,
        _ => ShareKind::File,
    };
    Ok(ShareSummary {
        id: r.get::<_, i64>("id")? as u64,
        name: r.get("name")?,
        kind,
        size: r.get::<_, i64>("size")? as u64,
        file_count: r.get::<_, i64>("file_count")? as u64,
        owner_name: r.get("owner_name")?,
        created_at: r.get("created_at")?,
    })
}

pub fn list_shares(conn: &Connection) -> rusqlite::Result<Vec<ShareSummary>> {
    let mut stmt = conn.prepare(
        "SELECT id,name,kind,size,file_count,owner_name,created_at FROM shares ORDER BY created_at DESC",
    )?;
    let rows = stmt
        .query_map([], row_to_share)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_share(conn: &Connection, id: u64) -> rusqlite::Result<Option<ShareSummary>> {
    conn.query_row(
        "SELECT id,name,kind,size,file_count,owner_name,created_at FROM shares WHERE id=?1",
        params![id as i64],
        row_to_share,
    )
    .optional()
}

/// `(stored_path, owner_client)` for a share — used to enforce delete rules and
/// locate files (F6.9).
pub fn share_stored_path(
    conn: &Connection,
    id: u64,
) -> rusqlite::Result<Option<(String, Option<String>)>> {
    conn.query_row(
        "SELECT stored_path, owner_client FROM shares WHERE id=?1",
        params![id as i64],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
    )
    .optional()
}

/// All current share `stored_path`s — the orphan sweep keeps only these under
/// the share root.
pub fn all_stored_paths(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT stored_path FROM shares")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn share_files(conn: &Connection, id: u64) -> rusqlite::Result<Vec<(String, u64)>> {
    let mut stmt =
        conn.prepare("SELECT rel_path,size FROM share_files WHERE share_id=?1 ORDER BY rel_path")?;
    let rows = stmt
        .query_map(params![id as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn delete_share(conn: &Connection, id: u64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM shares WHERE id=?1", params![id as i64])?;
    Ok(())
}

// ───────────────────────────── names ───────────────────────────────

pub fn set_name(conn: &Connection, client_id: &str, name: &str, at: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO names(client_id,name,updated_at) VALUES(?1,?2,?3)
         ON CONFLICT(client_id) DO UPDATE SET name=excluded.name, updated_at=excluded.updated_at",
        params![client_id, name, at],
    )?;
    Ok(())
}

pub fn get_name(conn: &Connection, client_id: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT name FROM names WHERE client_id=?1",
        params![client_id],
        |r| r.get::<_, String>(0),
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;
    use blt_core::hashing::hash_bytes;
    use blt_core::manifest::build_file_entry;

    #[test]
    fn title_and_manifest_roundtrip() {
        let mut conn = open_memory().unwrap();
        set_config(
            &conn,
            "chunk_size",
            &blt_core::DEFAULT_CHUNK_SIZE.to_string(),
        )
        .unwrap();
        let id = ensure_title(&conn, "Some Game").unwrap();
        assert_eq!(title_id_by_name(&conn, "Some Game").unwrap(), Some(id));

        let fe = build_file_entry(
            1,
            "bin/game.exe".into(),
            10,
            42,
            hash_bytes(b"file"),
            blt_core::DEFAULT_CHUNK_SIZE,
            &[hash_bytes(b"c0")],
        );
        replace_manifest(
            &mut conn,
            id,
            std::slice::from_ref(&fe),
            10,
            1,
            1,
            1000,
            None,
            None,
            false,
            false,
        )
        .unwrap();

        let t = get_title(&conn, id).unwrap().unwrap();
        assert_eq!(t.manifest_ver, 1);
        assert_eq!(t.file_count, 1);
        assert_eq!(t.state, "published");

        let m = load_manifest(&conn, id).unwrap().unwrap();
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].rel_path, "bin/game.exe");
        assert_eq!(m.files[0].chunks.len(), 1);
        assert_eq!(m.files[0].chunks[0].hash, hash_bytes(b"c0"));
    }

    #[test]
    fn label_overrides_and_state() {
        let conn = open_memory().unwrap();
        let id = ensure_title(&conn, "Folder Name").unwrap();
        set_title_label(&conn, id, Some("Pretty Name")).unwrap();
        let t = get_title(&conn, id).unwrap().unwrap();
        assert_eq!(t.label.as_deref(), Some("Pretty Name"));
        set_title_state(&conn, id, "removed").unwrap();
        assert!(list_titles(&conn).unwrap().is_empty());
    }

    #[test]
    fn shares_crud_and_files() {
        let mut conn = open_memory().unwrap();
        let id = insert_share(
            &mut conn,
            "My Folder",
            ShareKind::Folder,
            15,
            "Rick",
            Some("client-1"),
            "/share/My Folder",
            1234,
            &[("a.txt".into(), 5), ("sub/b.txt".into(), 10)],
        )
        .unwrap();
        let s = get_share(&conn, id).unwrap().unwrap();
        assert_eq!(s.file_count, 2);
        assert_eq!(s.kind, ShareKind::Folder);
        let files = share_files(&conn, id).unwrap();
        assert_eq!(files.len(), 2);
        let (path, owner) = share_stored_path(&conn, id).unwrap().unwrap();
        assert_eq!(path, "/share/My Folder");
        assert_eq!(owner.as_deref(), Some("client-1"));
        delete_share(&conn, id).unwrap();
        assert!(list_shares(&conn).unwrap().is_empty());
        // cascade removed share_files
        assert!(share_files(&conn, id).unwrap().is_empty());
    }

    #[test]
    fn names_upsert() {
        let conn = open_memory().unwrap();
        set_name(&conn, "c1", "Rick", 1).unwrap();
        set_name(&conn, "c1", "Rickk", 2).unwrap();
        assert_eq!(get_name(&conn, "c1").unwrap().as_deref(), Some("Rickk"));
    }
}
