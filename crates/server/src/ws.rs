//! The `/ws` live channel hub: session registry, roster, and the peer registry
//! that makes the server the P2P rendezvous (TDD §7, F13).
//!
//! Fan-out and targeted sends go through per-connection `mpsc` senders held in
//! the [`SessionRegistry`]. Session/activity state is **transient (in-memory)**;
//! only durable bits (the display-name mapping) live in SQLite (TDD §7.4).

use blt_core::protocol::{Mode, PeerEntry, RosterEntry, ServerMsg};
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedSender;

/// One connected websocket.
pub struct Conn {
    pub id: u64,
    pub client_id: String,
    pub display_name: String,
    pub machine_name: String,
    pub mode: Mode,
    pub activity: String,
    pub server_only: bool,
    pub throughput_bps: Option<u64>,
    pub tx: UnboundedSender<ServerMsg>,
}

/// All live connections + the per-title peer registry.
#[derive(Default)]
pub struct SessionRegistry {
    next_id: u64,
    conns: HashMap<u64, Conn>,
    /// `(title_id, manifest_ver)` → (`client_id` → `chunk_endpoint`).
    peers: HashMap<(u64, u32), HashMap<String, String>>,
}

impl SessionRegistry {
    /// Register a new connection, returning its connection id.
    pub fn add(&mut self, client_id: String, tx: UnboundedSender<ServerMsg>) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.conns.insert(
            id,
            Conn {
                id,
                client_id,
                display_name: String::new(),
                machine_name: String::new(),
                mode: Mode::Client,
                activity: "idle".into(),
                server_only: false,
                throughput_bps: None,
                tx,
            },
        );
        id
    }

    /// Remove a connection and any peer advertisements it held.
    pub fn remove(&mut self, conn_id: u64) {
        if let Some(c) = self.conns.remove(&conn_id) {
            // Drop this client's peer entries (unless another connection shares the id).
            let still_present = self.conns.values().any(|o| o.client_id == c.client_id);
            if !still_present {
                for set in self.peers.values_mut() {
                    set.remove(&c.client_id);
                }
                self.peers.retain(|_, set| !set.is_empty());
            }
        }
    }

    pub fn update_hello(
        &mut self,
        conn_id: u64,
        client_id: String,
        display_name: String,
        machine_name: String,
        mode: Mode,
    ) {
        if let Some(c) = self.conns.get_mut(&conn_id) {
            c.client_id = client_id;
            c.display_name = display_name;
            c.machine_name = machine_name;
            c.mode = mode;
        }
    }

    /// Whether `client_id` is currently flagged server-only (any connection).
    /// Server-only clients are excluded from peer lists in both directions —
    /// the failure must degrade gracefully, never silently (F13.5).
    fn is_server_only(&self, client_id: &str) -> bool {
        self.conns
            .values()
            .any(|c| c.client_id == client_id && c.server_only)
    }

    /// Up to `n` distinct peer chunk-endpoints for the reachability self-test
    /// (F13.4), excluding the probing client's own and server-only peers.
    pub fn probe_candidates(&self, exclude_client: &str, n: usize) -> Vec<PeerEntry> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for set in self.peers.values() {
            for (cid, ep) in set {
                if cid != exclude_client && !self.is_server_only(cid) && seen.insert(ep.clone()) {
                    out.push(PeerEntry {
                        client_id: cid.clone(),
                        chunk_endpoint: ep.clone(),
                    });
                    if out.len() >= n {
                        return out;
                    }
                }
            }
        }
        out
    }

    pub fn update_activity(
        &mut self,
        conn_id: u64,
        activity: String,
        throughput_bps: Option<u64>,
        server_only: bool,
    ) {
        if let Some(c) = self.conns.get_mut(&conn_id) {
            c.activity = activity;
            c.throughput_bps = throughput_bps;
            c.server_only = server_only;
        }
    }

    pub fn announce_peer(
        &mut self,
        client_id: &str,
        title_id: u64,
        manifest_ver: u32,
        chunk_endpoint: String,
    ) {
        self.peers
            .entry((title_id, manifest_ver))
            .or_default()
            .insert(client_id.to_string(), chunk_endpoint);
    }

    pub fn unannounce_peer(&mut self, client_id: &str, title_id: u64, manifest_ver: u32) {
        if let Some(set) = self.peers.get_mut(&(title_id, manifest_ver)) {
            set.remove(client_id);
            if set.is_empty() {
                self.peers.remove(&(title_id, manifest_ver));
            }
        }
    }

    /// Peers for a title, excluding the requesting client and any peer
    /// currently flagged server-only (F4.10, F13.5).
    pub fn peers_for(
        &self,
        title_id: u64,
        manifest_ver: u32,
        exclude_client: &str,
    ) -> Vec<PeerEntry> {
        self.peers
            .get(&(title_id, manifest_ver))
            .map(|set| {
                set.iter()
                    .filter(|(cid, _)| cid.as_str() != exclude_client && !self.is_server_only(cid))
                    .map(|(cid, ep)| PeerEntry {
                        client_id: cid.clone(),
                        chunk_endpoint: ep.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The current roster snapshot (F13.1), one entry per connection.
    pub fn roster(&self) -> Vec<RosterEntry> {
        let mut v: Vec<RosterEntry> = self
            .conns
            .values()
            .map(|c| RosterEntry {
                client_id: c.client_id.clone(),
                display_name: c.display_name.clone(),
                machine_name: c.machine_name.clone(),
                activity: c.activity.clone(),
                server_only: c.server_only,
                throughput_bps: c.throughput_bps,
            })
            .collect();
        v.sort_by(|a, b| a.display_name.cmp(&b.display_name));
        v
    }

    /// Send to every connection (drops dead senders silently; the read loop
    /// cleans them up on disconnect).
    pub fn broadcast(&self, msg: ServerMsg) {
        for c in self.conns.values() {
            let _ = c.tx.send(msg.clone());
        }
    }

    /// Send to a single connection by id.
    pub fn send_to_conn(&self, conn_id: u64, msg: ServerMsg) {
        if let Some(c) = self.conns.get(&conn_id) {
            let _ = c.tx.send(msg);
        }
    }

    /// Send to every connection belonging to `client_id`.
    pub fn send_to_client(&self, client_id: &str, msg: ServerMsg) {
        for c in self.conns.values().filter(|c| c.client_id == client_id) {
            let _ = c.tx.send(msg.clone());
        }
    }

    /// `(conn_id, client_id)` for each connection — used for personalised
    /// fan-out (e.g. per-client `voted_by_me` in the jukebox state).
    pub fn conns_snapshot(&self) -> Vec<(u64, String)> {
        self.conns
            .values()
            .map(|c| (c.id, c.client_id.clone()))
            .collect()
    }

    /// `(client_id, display_name, mode)` for one connection.
    pub fn conn_info(&self, conn_id: u64) -> Option<(String, String, Mode)> {
        self.conns
            .get(&conn_id)
            .map(|c| (c.client_id.clone(), c.display_name.clone(), c.mode))
    }

    pub fn conn_count(&self) -> usize {
        self.conns.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn peer_registry_excludes_self_and_cleans_on_remove() {
        let mut reg = SessionRegistry::default();
        let (tx1, _r1) = unbounded_channel();
        let (tx2, _r2) = unbounded_channel();
        let c1 = reg.add("client-1".into(), tx1);
        let _c2 = reg.add("client-2".into(), tx2);
        reg.announce_peer("client-1", 5, 1, "192.168.1.5:9000".into());
        reg.announce_peer("client-2", 5, 1, "192.168.1.6:9000".into());

        let peers = reg.peers_for(5, 1, "client-1");
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].client_id, "client-2");

        // removing client-1's connection drops its advertisement
        reg.remove(c1);
        let peers = reg.peers_for(5, 1, "client-2");
        assert_eq!(peers.len(), 0); // only client-1 had advertised to client-2's view
    }

    #[test]
    fn roster_reflects_activity() {
        let mut reg = SessionRegistry::default();
        let (tx, _r) = unbounded_channel();
        let id = reg.add("c1".into(), tx);
        reg.update_hello(
            id,
            "c1".into(),
            "Rick".into(),
            "rick-mac".into(),
            Mode::Client,
        );
        reg.update_activity(id, "downloading Doom".into(), Some(4_000_000), false);
        let roster = reg.roster();
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].display_name, "Rick");
        assert_eq!(roster[0].activity, "downloading Doom");
        assert_eq!(roster[0].throughput_bps, Some(4_000_000));
    }
}
