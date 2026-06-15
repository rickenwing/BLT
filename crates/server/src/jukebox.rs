//! Server-authoritative jukebox state machine (TDD §8, F8–F11).
//!
//! The queue/votes live in SQLite; ordering uses [`blt_core::jukebox`]. The
//! currently-playing item is **pinned** and never reordered (F8.6). External/DRM
//! items put the queue into `PlayingExternal` (awaiting-human, no auto-advance);
//! `Next` (human) is the way out (F10).

use blt_core::jukebox::{order_up_next, OrderMode, QueueItem};
use blt_core::protocol::{ItemType, JukeboxItemDto, JukeboxState, PlaybackState};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};

/// In-memory authoritative jukebox state (queue rows live in the DB).
pub struct Jukebox {
    pub mode: OrderMode,
    pub playback_state: PlaybackState,
    pub now_playing: Option<u64>,
}

impl Jukebox {
    pub fn new(mode: OrderMode) -> Self {
        Jukebox {
            mode,
            playback_state: PlaybackState::Idle,
            now_playing: None,
        }
    }

    /// Add an item to the queue; returns its id.
    #[allow(clippy::too_many_arguments)]
    pub fn add(
        &self,
        conn: &Connection,
        item_type: ItemType,
        reference: &str,
        title: Option<&str>,
        added_by: &str,
        added_by_id: &str,
        now: i64,
    ) -> rusqlite::Result<u64> {
        conn.execute(
            "INSERT INTO jukebox_items(type,ref,title,added_by,added_by_id,added_at,state)
             VALUES(?1,?2,?3,?4,?5,?6,'queued')",
            params![
                item_type.as_str(),
                reference,
                title,
                added_by,
                added_by_id,
                now
            ],
        )?;
        Ok(conn.last_insert_rowid() as u64)
    }

    /// Reject URL-bearing item refs that aren't http(s) (SEC-7): `direct_url`
    /// becomes a `<video src>` and `external` opens a browser on the playback
    /// machine — a client must not be able to point either at `file://` or an
    /// internal address.
    pub fn ref_acceptable(item_type: ItemType, reference: &str) -> bool {
        match item_type {
            ItemType::DirectUrl | ItemType::External => {
                let r = reference.trim();
                r.starts_with("http://") || r.starts_with("https://")
            }
            _ => true,
        }
    }

    /// Toggle a vote (F8.4); votes key on `client_id` so renaming can't
    /// double-vote. Returns the new voted state for this client; voting on a
    /// non-existent item is a no-op returning `false`. Real DB errors
    /// propagate — never collapsed into "not voted".
    pub fn toggle_vote(
        &self,
        conn: &Connection,
        item_id: u64,
        voter_id: &str,
    ) -> rusqlite::Result<bool> {
        let item_exists = conn
            .query_row(
                "SELECT 1 FROM jukebox_items WHERE id=?1",
                params![item_id as i64],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !item_exists {
            return Ok(false);
        }
        let voted = conn
            .query_row(
                "SELECT 1 FROM jukebox_votes WHERE item_id=?1 AND voter_id=?2",
                params![item_id as i64, voter_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if voted {
            conn.execute(
                "DELETE FROM jukebox_votes WHERE item_id=?1 AND voter_id=?2",
                params![item_id as i64, voter_id],
            )?;
            Ok(false)
        } else {
            conn.execute(
                "INSERT OR IGNORE INTO jukebox_votes(item_id,voter_id) VALUES(?1,?2)",
                params![item_id as i64, voter_id],
            )?;
            Ok(true)
        }
    }

    pub fn remove(&mut self, conn: &Connection, item_id: u64) -> rusqlite::Result<()> {
        conn.execute(
            "DELETE FROM jukebox_items WHERE id=?1",
            params![item_id as i64],
        )?;
        if self.now_playing == Some(item_id) {
            self.now_playing = None;
            self.playback_state = PlaybackState::Idle;
        }
        Ok(())
    }

    pub fn clear(&mut self, conn: &Connection) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM jukebox_items", [])?;
        self.now_playing = None;
        self.playback_state = PlaybackState::Idle;
        Ok(())
    }

    pub fn set_mode(&mut self, mode: OrderMode) {
        self.mode = mode;
    }

    /// Advance: mark the current item played, promote the next per the active
    /// ordering, and set playback state by the next item's type. Used by both
    /// human `Next` (any current type) and embedded `ended` (auto-advance).
    pub fn advance(&mut self, conn: &Connection) -> rusqlite::Result<()> {
        if let Some(cur) = self.now_playing.take() {
            conn.execute(
                "UPDATE jukebox_items SET state='played' WHERE id=?1",
                params![cur as i64],
            )?;
        }
        let order = self.up_next_order(conn)?;
        match order.first() {
            None => {
                self.playback_state = PlaybackState::Idle;
            }
            Some(&next_id) => {
                // Decide the playback state BEFORE committing now_playing so a
                // DB error can never leave the contradictory
                // `now_playing=Some + Idle` combination (TDD §8 has no such
                // state). An unrecognised type string (unreachable via our own
                // writes) degrades to embedded — the human can Next past it.
                let ty = item_type_of(conn, next_id)?;
                let next_state = match ty {
                    Some(t) if t.is_external() => PlaybackState::PlayingExternal,
                    _ => PlaybackState::PlayingEmbedded,
                };
                conn.execute(
                    "UPDATE jukebox_items SET state='playing' WHERE id=?1",
                    params![next_id as i64],
                )?;
                self.now_playing = Some(next_id);
                self.playback_state = next_state;
            }
        }
        Ok(())
    }

    /// Ordered up-next item ids (queued only): admin-overridden items first
    /// (ascending override), then the rest per the active mode (F8.5, F11.1).
    fn up_next_order(&self, conn: &Connection) -> rusqlite::Result<Vec<u64>> {
        let votes = vote_counts(conn)?;
        let mut items = Vec::new();
        let mut overridden: Vec<(i64, u64)> = Vec::new();
        let mut stmt = conn.prepare(
            "SELECT id,added_by_id,added_at,sort_override FROM jukebox_items WHERE state='queued'",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?;
        for row in rows {
            let (id, contributor, added_at, sort_override) = row?;
            match sort_override {
                Some(rank) => overridden.push((rank, id)),
                None => items.push(QueueItem {
                    id,
                    contributor,
                    votes: votes.get(&id).copied().unwrap_or(0),
                    added_at,
                }),
            }
        }
        overridden.sort();
        let mut order: Vec<u64> = overridden.into_iter().map(|(_, id)| id).collect();
        order.extend(order_up_next(&items, self.mode));
        Ok(order)
    }

    /// Admin reorder (F11.1): move an item to the head of up-next.
    pub fn move_to_top(&self, conn: &Connection, item_id: u64) -> rusqlite::Result<()> {
        let min: Option<i64> = conn.query_row(
            "SELECT MIN(sort_override) FROM jukebox_items WHERE state='queued'",
            [],
            |r| r.get(0),
        )?;
        conn.execute(
            "UPDATE jukebox_items SET sort_override=?2 WHERE id=?1",
            params![item_id as i64, min.unwrap_or(0) - 1],
        )?;
        Ok(())
    }

    /// Build the full state DTO for a given viewer (for `voted_by_me`).
    pub fn state_for(&self, conn: &Connection, viewer_id: &str) -> rusqlite::Result<JukeboxState> {
        let votes = vote_counts(conn)?;
        let my_votes = my_votes(conn, viewer_id)?;

        let now_playing = match self.now_playing {
            Some(id) => item_dto(conn, id, &votes, &my_votes)?,
            None => None,
        };
        let order = self.up_next_order(conn)?;
        let mut up_next = Vec::with_capacity(order.len());
        for id in order {
            if let Some(dto) = item_dto(conn, id, &votes, &my_votes)? {
                up_next.push(dto);
            }
        }
        Ok(JukeboxState {
            mode: self.mode,
            playback_state: self.playback_state,
            now_playing,
            up_next,
        })
    }
}

fn vote_counts(conn: &Connection) -> rusqlite::Result<HashMap<u64, u32>> {
    let mut stmt = conn.prepare("SELECT item_id, COUNT(*) FROM jukebox_votes GROUP BY item_id")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u32))
    })?;
    let mut m = HashMap::new();
    for row in rows {
        let (id, n) = row?;
        m.insert(id, n);
    }
    Ok(m)
}

fn my_votes(conn: &Connection, voter_id: &str) -> rusqlite::Result<HashSet<u64>> {
    let mut stmt = conn.prepare("SELECT item_id FROM jukebox_votes WHERE voter_id=?1")?;
    let rows = stmt.query_map(params![voter_id], |r| Ok(r.get::<_, i64>(0)? as u64))?;
    let mut s = HashSet::new();
    for row in rows {
        s.insert(row?);
    }
    Ok(s)
}

fn item_type_of(conn: &Connection, id: u64) -> rusqlite::Result<Option<ItemType>> {
    // .optional()? propagates real DB errors; only a genuinely-absent row (or
    // an unrecognised type string) yields None.
    let ty: Option<String> = conn
        .query_row(
            "SELECT type FROM jukebox_items WHERE id=?1",
            params![id as i64],
            |r| r.get(0),
        )
        .optional()?;
    Ok(ty.and_then(|s| ItemType::from_str_opt(&s)))
}

fn item_dto(
    conn: &Connection,
    id: u64,
    votes: &HashMap<u64, u32>,
    my_votes: &HashSet<u64>,
) -> rusqlite::Result<Option<JukeboxItemDto>> {
    let row = conn
        .query_row(
            "SELECT id,type,ref,title,added_by,added_by_id,added_at,state
             FROM jukebox_items WHERE id=?1",
            params![id as i64],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, String>(7)?,
                ))
            },
        )
        .optional()?;
    let Some((id, ty, reference, title, added_by, added_by_id, added_at, state)) = row else {
        return Ok(None);
    };
    let Some(item_type) = ItemType::from_str_opt(&ty) else {
        return Ok(None);
    };
    Ok(Some(JukeboxItemDto {
        id,
        item_type,
        reference,
        title,
        added_by,
        added_by_id,
        added_at,
        votes: votes.get(&id).copied().unwrap_or(0),
        voted_by_me: my_votes.contains(&id),
        state,
    }))
}

/// Reload the persisted ordering mode + restore any 'playing' item on startup.
pub fn restore(conn: &Connection, mode: OrderMode) -> rusqlite::Result<Jukebox> {
    let mut jb = Jukebox::new(mode);
    let playing: Option<u64> = conn
        .query_row(
            "SELECT id FROM jukebox_items WHERE state='playing' LIMIT 1",
            [],
            |r| r.get::<_, i64>(0).map(|v| v as u64),
        )
        .optional()?;
    if let Some(id) = playing {
        jb.now_playing = Some(id);
        jb.playback_state = match item_type_of(conn, id)? {
            Some(t) if t.is_external() => PlaybackState::PlayingExternal,
            _ => PlaybackState::PlayingEmbedded,
        };
    }
    Ok(jb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn now_seq() -> impl FnMut() -> i64 {
        let mut t = 1000i64;
        move || {
            t += 1;
            t
        }
    }

    #[test]
    fn add_vote_order_and_advance() {
        let conn = db::open_memory().unwrap();
        let mut jb = Jukebox::new(OrderMode::Votes);
        let mut now = now_seq();

        let a = jb
            .add(
                &conn,
                ItemType::Youtube,
                "yt:a",
                Some("A"),
                "Rick",
                "c1",
                now(),
            )
            .unwrap();
        let b = jb
            .add(
                &conn,
                ItemType::DirectUrl,
                "http:b",
                Some("B"),
                "Sam",
                "c2",
                now(),
            )
            .unwrap();

        // b gets 2 votes, a gets 1 → vote-ranked b first
        assert!(jb.toggle_vote(&conn, b, "c1").unwrap());
        assert!(jb.toggle_vote(&conn, b, "c3").unwrap());
        assert!(jb.toggle_vote(&conn, a, "c2").unwrap());

        let st = jb.state_for(&conn, "c1").unwrap();
        assert_eq!(st.up_next[0].id, b);
        assert_eq!(st.up_next[0].votes, 2);
        assert!(st.up_next[0].voted_by_me); // c1 voted b

        // toggle off
        assert!(!jb.toggle_vote(&conn, b, "c1").unwrap());
        let st = jb.state_for(&conn, "c1").unwrap();
        assert_eq!(st.up_next.iter().find(|i| i.id == b).unwrap().votes, 1);

        // advance: b (now 1 vote, added earlier than a? a older) — recompute.
        jb.advance(&conn).unwrap();
        assert!(jb.now_playing.is_some());
        assert_eq!(jb.playback_state, PlaybackState::PlayingEmbedded);
    }

    #[test]
    fn external_item_sets_awaiting_human() {
        let conn = db::open_memory().unwrap();
        let mut jb = Jukebox::new(OrderMode::Fair);
        let mut now = now_seq();
        jb.add(
            &conn,
            ItemType::External,
            "https://netflix.com/x",
            Some("Movie"),
            "Rick",
            "c1",
            now(),
        )
        .unwrap();
        jb.advance(&conn).unwrap();
        assert_eq!(jb.playback_state, PlaybackState::PlayingExternal);
        // Next from a human advances out of external; empty queue → idle
        jb.advance(&conn).unwrap();
        assert_eq!(jb.playback_state, PlaybackState::Idle);
        assert!(jb.now_playing.is_none());
    }

    #[test]
    fn vote_keyed_on_client_not_name() {
        let conn = db::open_memory().unwrap();
        let jb = Jukebox::new(OrderMode::Votes);
        let mut now = now_seq();
        let a = jb
            .add(&conn, ItemType::Youtube, "yt:a", None, "Rick", "c1", now())
            .unwrap();
        assert!(jb.toggle_vote(&conn, a, "c1").unwrap());
        // same client tries again (e.g. after rename) → toggles off, never 2 votes
        assert!(!jb.toggle_vote(&conn, a, "c1").unwrap());
        let st = jb.state_for(&conn, "c1").unwrap();
        assert_eq!(st.up_next[0].votes, 0);
    }
}
