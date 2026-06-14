//! Client SQLite (TDD §4.2): settings, known servers, per-title locations, and
//! resume state (the chunk bitmap). WAL mode; no `localStorage` is ever used
//! for durable state (CLAUDE.md conventions).

use blt_core::bitmap::Bitmap;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub fn open(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&conn)?;
    Ok(conn)
}

#[cfg(test)]
pub fn open_memory() -> anyhow::Result<Connection> {
    let conn = Connection::open_in_memory()?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        -- mode (client|playback), playback_locked, display_name, client_id,
        -- default_download_root, upload_cap_bytes_per_sec, share_back, last_server

        CREATE TABLE IF NOT EXISTS servers (
          id        INTEGER PRIMARY KEY,
          uuid      TEXT UNIQUE,
          label     TEXT,
          game_endpoint   TEXT,
          share_endpoint  TEXT,
          last_seen INTEGER
        );

        CREATE TABLE IF NOT EXISTS title_locations (
          title_id  INTEGER PRIMARY KEY,
          dest_path TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS download_state (
          title_id      INTEGER NOT NULL,
          manifest_ver  INTEGER NOT NULL,
          chunk_count   INTEGER NOT NULL,
          chunk_bitmap  BLOB NOT NULL,
          status        TEXT NOT NULL,   -- queued|active|paused|complete|error
          dest_path     TEXT NOT NULL,
          error         TEXT,
          PRIMARY KEY (title_id, manifest_ver)
        );
        "#,
    )?;
    Ok(())
}

// ── settings KV ──

pub fn get_setting(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM settings WHERE key=?1",
        params![key],
        |r| r.get(0),
    )
    .optional()
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings(key,value) VALUES(?1,?2)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

// ── servers ──

#[derive(Debug, Clone, serde::Serialize)]
pub struct ServerRow {
    pub id: i64,
    pub uuid: Option<String>,
    pub label: Option<String>,
    pub game_endpoint: Option<String>,
    pub share_endpoint: Option<String>,
    pub last_seen: i64,
}

pub fn upsert_server(
    conn: &Connection,
    uuid: Option<&str>,
    label: &str,
    game: &str,
    share: &str,
    now: i64,
) -> rusqlite::Result<()> {
    match uuid {
        // Discovered (mDNS): adopt any manual (uuid-less) duplicate of the same
        // endpoint so the same box never shows twice (BUG-2), then upsert by uuid.
        Some(u) => {
            conn.execute(
                "DELETE FROM servers WHERE uuid IS NULL AND game_endpoint=?1",
                params![game],
            )?;
            conn.execute(
                "INSERT INTO servers(uuid,label,game_endpoint,share_endpoint,last_seen)
                 VALUES(?1,?2,?3,?4,?5)
                 ON CONFLICT(uuid) DO UPDATE SET label=?2, game_endpoint=?3, share_endpoint=?4, last_seen=?5",
                params![u, label, game, share, now],
            )?;
        }
        // Manual: dedup on endpoint — if any row (manual or discovered) already
        // has it, just refresh last_seen; never pile up duplicates (BUG-2). Don't
        // touch label/share on a match so a discovered row keeps its good data.
        None => {
            let touched = conn.execute(
                "UPDATE servers SET last_seen=?2 WHERE game_endpoint=?1",
                params![game, now],
            )?;
            if touched == 0 {
                conn.execute(
                    "INSERT INTO servers(uuid,label,game_endpoint,share_endpoint,last_seen)
                     VALUES(NULL,?1,?2,?3,?4)",
                    params![label, game, share, now],
                )?;
            }
        }
    }
    Ok(())
}

pub fn list_servers(conn: &Connection) -> rusqlite::Result<Vec<ServerRow>> {
    let mut stmt = conn.prepare(
        "SELECT id,uuid,label,game_endpoint,share_endpoint,last_seen
         FROM servers ORDER BY last_seen DESC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ServerRow {
                id: r.get(0)?,
                uuid: r.get(1)?,
                label: r.get(2)?,
                game_endpoint: r.get(3)?,
                share_endpoint: r.get(4)?,
                last_seen: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ── per-title destination ──

pub fn title_location(conn: &Connection, title_id: u64) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT dest_path FROM title_locations WHERE title_id=?1",
        params![title_id as i64],
        |r| r.get(0),
    )
    .optional()
}

pub fn set_title_location(conn: &Connection, title_id: u64, dest: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO title_locations(title_id,dest_path) VALUES(?1,?2)
         ON CONFLICT(title_id) DO UPDATE SET dest_path=excluded.dest_path",
        params![title_id as i64, dest],
    )?;
    Ok(())
}

// ── download/resume state ──

#[derive(Debug, Clone, serde::Serialize)]
pub struct DownloadRow {
    pub title_id: u64,
    pub manifest_ver: u32,
    pub chunk_count: u64,
    pub have_chunks: u64,
    pub status: String,
    pub dest_path: String,
    pub error: Option<String>,
}

pub fn save_download(
    conn: &Connection,
    title_id: u64,
    manifest_ver: u32,
    bitmap: &Bitmap,
    status: &str,
    dest_path: &str,
    error: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO download_state(title_id,manifest_ver,chunk_count,chunk_bitmap,status,dest_path,error)
         VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(title_id,manifest_ver) DO UPDATE SET
           chunk_count=excluded.chunk_count, chunk_bitmap=excluded.chunk_bitmap,
           status=excluded.status, dest_path=excluded.dest_path, error=excluded.error",
        params![
            title_id as i64,
            manifest_ver as i64,
            bitmap.len() as i64,
            bitmap.as_bytes(),
            status,
            dest_path,
            error
        ],
    )?;
    Ok(())
}

pub fn load_bitmap(
    conn: &Connection,
    title_id: u64,
    manifest_ver: u32,
) -> rusqlite::Result<Option<(Bitmap, String, String)>> {
    conn.query_row(
        "SELECT chunk_count, chunk_bitmap, status, dest_path FROM download_state
         WHERE title_id=?1 AND manifest_ver=?2",
        params![title_id as i64, manifest_ver as i64],
        |r| {
            let count: i64 = r.get(0)?;
            let bytes: Vec<u8> = r.get(1)?;
            Ok((
                Bitmap::from_bytes(count as u64, bytes),
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        },
    )
    .optional()
}

pub fn list_downloads(conn: &Connection) -> rusqlite::Result<Vec<DownloadRow>> {
    let mut stmt = conn.prepare(
        "SELECT title_id,manifest_ver,chunk_count,chunk_bitmap,status,dest_path,error
         FROM download_state ORDER BY title_id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let count: i64 = r.get(2)?;
            let bytes: Vec<u8> = r.get(3)?;
            let bm = Bitmap::from_bytes(count as u64, bytes);
            Ok(DownloadRow {
                title_id: r.get::<_, i64>(0)? as u64,
                manifest_ver: r.get::<_, i64>(1)? as u32,
                chunk_count: count as u64,
                have_chunks: bm.count_set(),
                status: r.get(4)?,
                dest_path: r.get(5)?,
                error: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn delete_download(
    conn: &Connection,
    title_id: u64,
    manifest_ver: u32,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM download_state WHERE title_id=?1 AND manifest_ver=?2",
        params![title_id as i64, manifest_ver as i64],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip() {
        let conn = open_memory().unwrap();
        assert_eq!(get_setting(&conn, "mode").unwrap(), None);
        set_setting(&conn, "mode", "client").unwrap();
        set_setting(&conn, "mode", "playback").unwrap();
        assert_eq!(
            get_setting(&conn, "mode").unwrap().as_deref(),
            Some("playback")
        );
    }

    #[test]
    fn bitmap_resume_roundtrip() {
        let conn = open_memory().unwrap();
        let mut bm = Bitmap::new(10);
        bm.set(3);
        bm.set(7);
        save_download(&conn, 5, 2, &bm, "paused", "/dest", None).unwrap();
        let (loaded, status, dest) = load_bitmap(&conn, 5, 2).unwrap().unwrap();
        assert_eq!(loaded, bm);
        assert_eq!(status, "paused");
        assert_eq!(dest, "/dest");
        // keyed by (title, version): other version is absent (F4.9)
        assert!(load_bitmap(&conn, 5, 3).unwrap().is_none());

        let rows = list_downloads(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].have_chunks, 2);
    }

    #[test]
    fn servers_upsert_by_uuid() {
        let conn = open_memory().unwrap();
        upsert_server(
            &conn,
            Some("u1"),
            "LAN",
            "10.0.0.1:7400",
            "10.0.0.1:7401",
            1,
        )
        .unwrap();
        upsert_server(
            &conn,
            Some("u1"),
            "LAN2",
            "10.0.0.2:7400",
            "10.0.0.2:7401",
            2,
        )
        .unwrap();
        let rows = list_servers(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label.as_deref(), Some("LAN2"));
    }

    #[test]
    fn servers_dedup_manual_and_discovered_on_endpoint() {
        let conn = open_memory().unwrap();
        // Repeated manual connects to the same endpoint must not pile up (BUG-2).
        upsert_server(&conn, None, "10.0.0.1:7400", "10.0.0.1:7400", "", 1).unwrap();
        upsert_server(&conn, None, "10.0.0.1:7400", "10.0.0.1:7400", "", 2).unwrap();
        assert_eq!(list_servers(&conn).unwrap().len(), 1);
        // Discovery of the same box adopts the manual row (no duplicate).
        upsert_server(
            &conn,
            Some("u1"),
            "LAN",
            "10.0.0.1:7400",
            "10.0.0.1:7401",
            3,
        )
        .unwrap();
        let rows = list_servers(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].uuid.as_deref(), Some("u1"));
        assert_eq!(rows[0].label.as_deref(), Some("LAN"));
    }
}
