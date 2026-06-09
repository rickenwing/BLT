//! Jukebox up-next ordering — the two admin-selectable modes (F8.5), pure logic.
//!
//! The currently-playing item is **pinned** by the caller and never passed here;
//! this orders only the up-next (`queued`) items. Votes are keyed on
//! `client_id` upstream (HARD CONSTRAINT #13); here a vote is just a count.

use serde::{Deserialize, Serialize};

/// A queued item the ordering operates on. `contributor` is the adder's
/// `client_id`; `votes` is the current upvote count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    pub id: u64,
    pub contributor: String,
    pub votes: u32,
    pub added_at: i64,
}

/// The two ordering modes; default is Fair Rotation (F8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderMode {
    #[default]
    Fair,
    Votes,
}

impl OrderMode {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderMode::Fair => "fair",
            OrderMode::Votes => "votes",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "votes" => OrderMode::Votes,
            _ => OrderMode::Fair,
        }
    }
}

/// Compare two items by "strength": higher votes first, then earlier add time,
/// then lower id (final deterministic tie-break).
fn stronger(a: &QueueItem, b: &QueueItem) -> std::cmp::Ordering {
    b.votes
        .cmp(&a.votes)
        .then(a.added_at.cmp(&b.added_at))
        .then(a.id.cmp(&b.id))
}

/// Order the up-next list per `mode`, returning item ids in play order.
pub fn order_up_next(items: &[QueueItem], mode: OrderMode) -> Vec<u64> {
    match mode {
        OrderMode::Votes => vote_ranked(items),
        OrderMode::Fair => fair_rotation(items),
    }
}

/// `ORDER BY votes DESC, added_at ASC` across all items (F8.5 Vote-Ranked).
fn vote_ranked(items: &[QueueItem]) -> Vec<u64> {
    let mut v: Vec<&QueueItem> = items.iter().collect();
    v.sort_by(|a, b| stronger(a, b));
    v.into_iter().map(|i| i.id).collect()
}

/// Round-robin by contributor; within a contributor's turn, highest-voted first
/// (ties → earlier add). The rotation order is **fixed** for the computation
/// (contributors ranked once by their strongest item) and then cycled — a true
/// round-robin. Re-ranking lanes every round would let a contributor play
/// back-to-back across a round boundary while others still had items queued,
/// which is exactly the hogging F8.5 exists to prevent.
fn fair_rotation(items: &[QueueItem]) -> Vec<u64> {
    use std::collections::HashMap;

    // Group items by contributor, each group sorted strongest-first.
    let mut groups: HashMap<&str, Vec<&QueueItem>> = HashMap::new();
    for it in items {
        groups.entry(it.contributor.as_str()).or_default().push(it);
    }
    for g in groups.values_mut() {
        g.sort_by(|a, b| stronger(a, b));
    }
    // Fix the rotation order: by each contributor's strongest item, then by
    // contributor id as a deterministic tie-break.
    let mut lanes: Vec<(&str, Vec<&QueueItem>)> = groups.into_iter().collect();
    lanes.sort_by(|(ka, va), (kb, vb)| stronger(va[0], vb[0]).then(ka.cmp(kb)));

    let mut out = Vec::with_capacity(items.len());
    let mut cursor = vec![0usize; lanes.len()];
    loop {
        let mut took_any = false;
        for (i, (_, v)) in lanes.iter().enumerate() {
            if cursor[i] < v.len() {
                out.push(v[cursor[i]].id);
                cursor[i] += 1;
                took_any = true;
            }
        }
        if !took_any {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u64, who: &str, votes: u32, added_at: i64) -> QueueItem {
        QueueItem {
            id,
            contributor: who.into(),
            votes,
            added_at,
        }
    }

    #[test]
    fn fair_rotation_interleaves_no_hogging() {
        // The spec example: A added three, B added one → A, B, A, A.
        // Equal votes so add-time decides; A's items added before B's.
        let items = vec![
            item(1, "A", 0, 10),
            item(2, "A", 0, 11),
            item(3, "A", 0, 12),
            item(4, "B", 0, 13),
        ];
        let order = order_up_next(&items, OrderMode::Fair);
        // ids: A1, B4, A2, A3
        assert_eq!(order, vec![1, 4, 2, 3]);
    }

    #[test]
    fn fair_rotation_highest_voted_first_within_contributor() {
        // A's three items, middle one most-voted → it leads A's turns.
        let items = vec![
            item(1, "A", 1, 10),
            item(2, "A", 5, 11),
            item(3, "A", 3, 12),
            item(4, "B", 0, 9),
        ];
        let order = order_up_next(&items, OrderMode::Fair);
        // Round 1 heads: A's strongest is id2(votes5), B's id4(votes0) →
        // A before B. Then A's remaining: id3(3), id1(1).
        // => A2, B4, A3, A1
        assert_eq!(order, vec![2, 4, 3, 1]);
    }

    #[test]
    fn fair_single_contributor_is_just_their_vote_order() {
        let items = vec![
            item(1, "A", 1, 10),
            item(2, "A", 9, 11),
            item(3, "A", 5, 12),
        ];
        assert_eq!(order_up_next(&items, OrderMode::Fair), vec![2, 3, 1]);
    }

    #[test]
    fn fair_rotation_never_back_to_back_while_others_queued() {
        // Regression: re-ranking lanes each round let A play twice in a row
        // across a round boundary while B still had items.
        // A: votes [5, 4]; B: votes [9, 0]. Fixed rotation: B first (head 9).
        let items = vec![
            item(1, "A", 5, 10),
            item(2, "A", 4, 11),
            item(3, "B", 9, 12),
            item(4, "B", 0, 13),
        ];
        let order = order_up_next(&items, OrderMode::Fair);
        // B(3), A(1), B(4), A(2) — strict alternation, no hogging.
        assert_eq!(order, vec![3, 1, 4, 2]);
        // Property: no two adjacent items share a contributor while the other
        // still has items queued.
        let by_id = |id: u64| items.iter().find(|i| i.id == id).unwrap();
        for w in order.windows(2) {
            let (a, b) = (by_id(w[0]), by_id(w[1]));
            if a.contributor == b.contributor {
                // only allowed when the other contributor is exhausted
                let other_left = items
                    .iter()
                    .filter(|i| i.contributor != a.contributor)
                    .filter(|i| {
                        order.iter().position(|&x| x == i.id).unwrap()
                            > order.iter().position(|&x| x == w[0]).unwrap()
                    })
                    .count();
                assert_eq!(other_left, 0, "back-to-back with items left: {order:?}");
            }
        }
    }

    #[test]
    fn vote_ranked_ignores_contributor() {
        let items = vec![
            item(1, "A", 1, 10),
            item(2, "A", 2, 11),
            item(3, "B", 5, 12),
            item(4, "B", 2, 9),
        ];
        // votes desc, then added_at asc: id3(5), id4(2,@9), id2(2,@11), id1(1)
        assert_eq!(order_up_next(&items, OrderMode::Votes), vec![3, 4, 2, 1]);
    }

    #[test]
    fn empty_queue_orders_empty() {
        assert!(order_up_next(&[], OrderMode::Fair).is_empty());
        assert!(order_up_next(&[], OrderMode::Votes).is_empty());
    }

    #[test]
    fn order_mode_serde_and_parse() {
        assert_eq!(OrderMode::default(), OrderMode::Fair);
        assert_eq!(OrderMode::from_str_lossy("votes"), OrderMode::Votes);
        assert_eq!(OrderMode::from_str_lossy("anything"), OrderMode::Fair);
        assert_eq!(serde_json::to_string(&OrderMode::Fair).unwrap(), "\"fair\"");
    }
}
