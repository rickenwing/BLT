//! Server-authoritative jukebox state machine (TDD §8, F8–F11).
//!
//! The queue/votes live in SQLite; ordering uses [`blt_core::jukebox`]. The
//! currently-playing item is **pinned** and never reordered (F8.6). External/DRM
//! items put the queue into `PlayingExternal` (awaiting-human, no auto-advance);
//! `Next` (human) is the way out (F10).

use blt_core::jukebox::{OrderMode, QueueItem, order_up_next};
use blt_core::protocol::{ItemType, JukeboxItemDto, JukeboxState, PlaybackState};
use rusqlite::{Connection, OptionalExtension, params};
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

    /// Number of items waiting in the queue (state='queued'). Used to cap the
    /// queue so a client can't spam `JukeboxAdd` into an unbounded DB/broadcast.
    pub fn queued_count(&self, conn: &Connection) -> rusqlite::Result<u64> {
        conn.query_row(
            "SELECT COUNT(*) FROM jukebox_items WHERE state='queued'",
            [],
            |r| r.get::<_, i64>(0).map(|n| n as u64),
        )
    }

    /// Validate an item ref per its type (SEC hardening). Every type is checked
    /// so a client can't smuggle a hostile ref over the unauthenticated WS:
    /// - `direct_url`/`youtube`/`external` must be http(s) — they become a media
    ///   source or open a browser on the playback machine; never `file://` or an
    ///   internal scheme. (`youtube` is a full URL; the playback side extracts the
    ///   id and the loopback embed proxy charset-sanitises it, so the id can't
    ///   inject into the embed page.)
    /// - `shared_file` must be a numeric share id (it's resolved server-side).
    pub fn ref_acceptable(item_type: ItemType, reference: &str) -> bool {
        let r = reference.trim();
        match item_type {
            ItemType::DirectUrl | ItemType::External | ItemType::Youtube => {
                r.starts_with("http://") || r.starts_with("https://")
            }
            ItemType::SharedFile => r.parse::<u64>().is_ok(),
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

    /// Remove every queue item that references a (now-deleted) shared-pool
    /// entry, so deleting a share also clears it from the rotation instead of
    /// leaving a dangling `shared_file` item that fails when it tries to play.
    /// Returns the number removed. A `shared_file` item stores the share id in
    /// `ref` as a string (mirrors how it's added/resolved). If the now-playing
    /// item is one of them, `remove` resets playback to idle.
    pub fn remove_share_refs(
        &mut self,
        conn: &Connection,
        share_id: u64,
    ) -> rusqlite::Result<usize> {
        let key = share_id.to_string();
        let ids: Vec<u64> = {
            let mut stmt =
                conn.prepare("SELECT id FROM jukebox_items WHERE type='shared_file' AND ref=?1")?;
            let rows = stmt.query_map(params![key], |r| r.get::<_, i64>(0).map(|v| v as u64))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for id in &ids {
            self.remove(conn, *id)?;
        }
        Ok(ids.len())
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

    /// The (last) playback machine disconnected while something was playing:
    /// nothing is actually rendering anymore, so clear the phantom now-playing
    /// (HARD CONSTRAINT #12 — never present stale state as current) and requeue
    /// that item at the **top** of up-next so it resumes when playback returns.
    /// Returns whether anything changed (false if nothing was playing).
    pub fn requeue_now_playing(&mut self, conn: &Connection) -> rusqlite::Result<bool> {
        let Some(cur) = self.now_playing.take() else {
            return Ok(false);
        };
        // Back to 'queued', pinned to the head (mirrors `move_to_top`).
        let min: Option<i64> = conn.query_row(
            "SELECT MIN(sort_override) FROM jukebox_items WHERE state='queued'",
            [],
            |r| r.get(0),
        )?;
        conn.execute(
            "UPDATE jukebox_items SET state='queued', sort_override=?2 WHERE id=?1",
            params![cur as i64, min.unwrap_or(0) - 1],
        )?;
        self.playback_state = PlaybackState::Idle;
        Ok(true)
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

    #[test]
    fn deleting_share_clears_its_jukebox_refs() {
        let conn = db::open_memory().unwrap();
        let mut jb = Jukebox::new(OrderMode::Fair);
        let mut now = now_seq();

        // Two queue items reference share 42; a YouTube item and a shared-file
        // item for a *different* share must survive the cascade.
        let s1 = jb
            .add(
                &conn,
                ItemType::SharedFile,
                "42",
                Some("clip.mp4"),
                "Rick",
                "c1",
                now(),
            )
            .unwrap();
        let _s2 = jb
            .add(
                &conn,
                ItemType::SharedFile,
                "42",
                Some("clip.mp4"),
                "Sam",
                "c2",
                now(),
            )
            .unwrap();
        let yt = jb
            .add(
                &conn,
                ItemType::Youtube,
                "yt:keep",
                Some("song"),
                "Rick",
                "c1",
                now(),
            )
            .unwrap();
        let other = jb
            .add(
                &conn,
                ItemType::SharedFile,
                "7",
                Some("other.mp4"),
                "Sam",
                "c2",
                now(),
            )
            .unwrap();

        // Share 42's first item happens to be playing right now.
        jb.now_playing = Some(s1);
        jb.playback_state = PlaybackState::PlayingEmbedded;

        let removed = jb.remove_share_refs(&conn, 42).unwrap();
        assert_eq!(removed, 2, "both items pointing at share 42 are removed");

        // The now-playing item was one of them → playback resets to idle.
        assert!(jb.now_playing.is_none());
        assert_eq!(jb.playback_state, PlaybackState::Idle);

        // Unrelated items (other share + YouTube) remain queued.
        let st = jb.state_for(&conn, "c1").unwrap();
        let ids: Vec<u64> = st.up_next.iter().map(|i| i.id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&yt));
        assert!(ids.contains(&other));

        // Deleting a share that nothing references is a no-op.
        assert_eq!(jb.remove_share_refs(&conn, 999).unwrap(), 0);
    }

    #[test]
    fn playback_disconnect_requeues_now_playing_on_top() {
        let conn = db::open_memory().unwrap();
        let mut jb = Jukebox::new(OrderMode::Fair);
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
                ItemType::Youtube,
                "yt:b",
                Some("B"),
                "Sam",
                "c2",
                now(),
            )
            .unwrap();

        // Start playing → one item is now-playing, the other queued.
        jb.advance(&conn).unwrap();
        let playing = jb.now_playing.unwrap();
        assert_eq!(jb.playback_state, PlaybackState::PlayingEmbedded);

        // Playback machine quits mid-play.
        assert!(jb.requeue_now_playing(&conn).unwrap());
        assert!(jb.now_playing.is_none());
        assert_eq!(jb.playback_state, PlaybackState::Idle);

        // No phantom now-playing; the interrupted item is back, pinned on top.
        let st = jb.state_for(&conn, "c1").unwrap();
        assert!(st.now_playing.is_none());
        assert_eq!(
            st.up_next.first().unwrap().id,
            playing,
            "interrupted item resumes from the top"
        );
        let ids: Vec<u64> = st.up_next.iter().map(|i| i.id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&a) && ids.contains(&b));

        // Idle with nothing playing → no-op.
        assert!(!jb.requeue_now_playing(&conn).unwrap());
    }

    #[test]
    fn ref_validation_rejects_hostile_refs() {
        use ItemType::*;
        // Accepted: http(s) media/external refs and a numeric share id.
        assert!(Jukebox::ref_acceptable(DirectUrl, "https://host/v.mp4"));
        assert!(Jukebox::ref_acceptable(
            Youtube,
            "https://youtu.be/dQw4w9WgXcQ"
        ));
        assert!(Jukebox::ref_acceptable(
            External,
            "http://netflix.com/title/1"
        ));
        assert!(Jukebox::ref_acceptable(SharedFile, "42"));
        // Rejected: non-http(s) schemes, file/internal, non-numeric share id.
        assert!(!Jukebox::ref_acceptable(DirectUrl, "file:///etc/passwd"));
        assert!(!Jukebox::ref_acceptable(External, "javascript:alert(1)"));
        assert!(!Jukebox::ref_acceptable(Youtube, "file:///x"));
        assert!(!Jukebox::ref_acceptable(SharedFile, "1; DROP TABLE"));
        assert!(!Jukebox::ref_acceptable(SharedFile, "../../etc"));
    }
}
