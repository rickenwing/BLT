//! P2P scheduling primitives — pure, deterministic, and fully unit-testable.
//!
//! The networking (a tiny HTTP chunk server per client, the WS peer registry)
//! lives in the server/desktop crates; the *decisions* live here:
//!
//! - [`TokenBucket`] — the client upload rate cap (default 10 MB/s, F4.11).
//! - [`Ewma`] / [`PeerRate`] — measured per-peer delivery rate (F13.6), no
//!   synthetic benchmark; band differences manifest only as a slower number.
//! - [`assign`] — the throughput-weighted chunk scheduler (F13.7): the server
//!   is the always-available baseline and takes the bulk; faster peers get more
//!   chunk requests; peers that are `server_only`, unreachable, or below a floor
//!   contribute nothing.

use std::collections::{HashMap, HashSet};

/// Default client upload (seed) cap: 10 MB/s = 10,485,760 B/s (F4.11). The UI
/// lets the user raise this up to an 80 MB/s ceiling.
pub const DEFAULT_UPLOAD_CAP_BPS: u64 = 10 * 1024 * 1024;

/// Baseline peer throughput the *download scheduler* assumes for weighting (a
/// conservative "typical capped peer"): the server is weighted well above it,
/// and an unmeasured peer bootstraps at it until a real measurement replaces it.
/// Deliberately kept independent of [`DEFAULT_UPLOAD_CAP_BPS`] — the server's own
/// throughput doesn't change when a peer raises its seed cap, so raising that cap
/// must not distort how download chunks are split between server and peers.
const SCHED_PEER_BASELINE_BPS: f64 = 1_572_864.0; // 1.5 MiB/s

/// A token bucket for rate-capping uploads. Time is injected via
/// [`TokenBucket::refill`] so behaviour is deterministic in tests; a real
/// seeder calls `refill` with the elapsed wall-clock since the last call.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    rate: f64,   // tokens (bytes) per second
    burst: f64,  // max tokens held
    tokens: f64, // current tokens
}

impl TokenBucket {
    /// `rate_bps` bytes/sec. The burst is sized to hold at least one default
    /// chunk (4 MiB) so `try_take(chunk_len)` can always eventually succeed —
    /// a burst smaller than the largest request would livelock the seeder
    /// (e.g. a low cap of 256 KiB/s < chunk 4 MiB).
    pub fn new(rate_bps: f64) -> Self {
        let rate = rate_bps.max(0.0);
        let burst = rate.max(crate::chunking::DEFAULT_CHUNK_SIZE as f64);
        TokenBucket {
            rate,
            burst,
            tokens: burst,
        }
    }

    pub fn with_burst(rate_bps: f64, burst: f64) -> Self {
        TokenBucket {
            rate: rate_bps.max(0.0),
            burst: burst.max(1.0),
            tokens: burst.max(1.0),
        }
    }

    /// Add tokens for `elapsed_secs` of elapsed time, capped at `burst`.
    pub fn refill(&mut self, elapsed_secs: f64) {
        if elapsed_secs > 0.0 {
            self.tokens = (self.tokens + self.rate * elapsed_secs).min(self.burst);
        }
    }

    /// Try to consume `n` bytes; returns true and deducts on success.
    pub fn try_take(&mut self, n: f64) -> bool {
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }

    /// Seconds until `n` tokens are available at the current rate (0 if already).
    /// Returns `f64::INFINITY` when the request can **never** be satisfied —
    /// zero rate, or `n` larger than the burst capacity (callers must not spin
    /// on an impossible request).
    pub fn time_until(&self, n: f64) -> f64 {
        if self.tokens >= n {
            0.0
        } else if self.rate <= 0.0 || n > self.burst {
            f64::INFINITY
        } else {
            (n - self.tokens) / self.rate
        }
    }

    pub fn available(&self) -> f64 {
        self.tokens
    }
}

/// Exponentially-weighted moving average.
#[derive(Debug, Clone)]
pub struct Ewma {
    alpha: f64,
    value: Option<f64>,
}

impl Ewma {
    /// `alpha` in (0,1]; higher reacts faster to recent samples.
    pub fn new(alpha: f64) -> Self {
        Ewma {
            alpha: alpha.clamp(f64::MIN_POSITIVE, 1.0),
            value: None,
        }
    }

    pub fn record(&mut self, sample: f64) {
        self.value = Some(match self.value {
            None => sample,
            Some(v) => self.alpha * sample + (1.0 - self.alpha) * v,
        });
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }
}

/// Measured delivery rate from one peer, as bytes/sec EWMA over received chunks.
#[derive(Debug, Clone)]
pub struct PeerRate(Ewma);

impl Default for PeerRate {
    fn default() -> Self {
        // ~last 10 chunks weighting.
        PeerRate(Ewma::new(0.2))
    }
}

impl PeerRate {
    /// Record a received chunk: `bytes` over `elapsed_secs`. A non-positive
    /// elapsed is ignored (avoids div-by-zero / infinite rates).
    pub fn record(&mut self, bytes: u64, elapsed_secs: f64) {
        if elapsed_secs > 0.0 {
            self.0.record(bytes as f64 / elapsed_secs);
        }
    }

    /// Current measured throughput in bytes/sec, or `None` if no samples yet.
    pub fn bytes_per_sec(&self) -> Option<f64> {
        self.0.value()
    }
}

/// A peer the scheduler may draw chunks from.
#[derive(Debug, Clone)]
pub struct PeerSource {
    pub id: String,
    /// Measured throughput in bytes/sec; `None` = not yet measured. The
    /// distinction matters: an **unmeasured** peer gets an optimistic bootstrap
    /// weight so measurement can start, while a peer **measured** below the
    /// floor contributes nothing (F13.7). Conflating the two would deadlock
    /// the swarm: no chunks → no measurement → no chunks, forever.
    pub throughput_bps: Option<f64>,
    /// Reachability self-test passed (F13.4).
    pub reachable: bool,
    /// Flagged server-only by the reachability self-test (F13.5) → never used.
    pub server_only: bool,
    /// Global chunk indices this peer can currently serve (from its bitmap).
    pub have: HashSet<u64>,
}

/// Scheduler tuning.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Baseline weight for the server so it takes the bulk of requests (F13.7).
    pub server_weight: f64,
    /// Peers measured below this many bytes/sec contribute nothing.
    pub peer_floor_bps: f64,
    /// Weight given to a reachable peer that has no measurement yet, so the
    /// first chunks flow and produce one. Modest: the capped client default.
    pub bootstrap_weight: f64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        // Server weight well above a single capped peer so the server dominates
        // while peers still offload a visible slice. Anchored to the scheduler
        // baseline (not the seed cap) so a higher seed cap doesn't shrink P2P's
        // share — see SCHED_PEER_BASELINE_BPS.
        SchedulerConfig {
            server_weight: 8.0 * SCHED_PEER_BASELINE_BPS,
            peer_floor_bps: 32.0 * 1024.0, // 32 KB/s
            bootstrap_weight: SCHED_PEER_BASELINE_BPS,
        }
    }
}

/// The fixed id used for the server source in assignments.
pub const SERVER_SOURCE_ID: &str = "@server";

/// Assign each missing chunk to a source, weighted by measured throughput, with
/// the server as the always-available baseline (F13.7). Deterministic.
///
/// Uses weighted least-virtual-time selection: each source has a virtual clock
/// advanced by `1/weight` per assigned chunk; the lowest-clock eligible source
/// wins each chunk. A source with twice the weight is picked ~twice as often.
pub fn assign(missing: &[u64], peers: &[PeerSource], cfg: &SchedulerConfig) -> Vec<(u64, String)> {
    // Effective weight per peer: unmeasured → bootstrap weight (so measurement
    // can start); measured below the floor → excluded; else the measurement.
    let eligible: Vec<(&PeerSource, f64)> = peers
        .iter()
        .filter(|p| p.reachable && !p.server_only)
        .filter_map(|p| match p.throughput_bps {
            None => Some((p, cfg.bootstrap_weight.max(1.0))),
            Some(bps) if bps >= cfg.peer_floor_bps => Some((p, bps)),
            Some(_) => None, // measured slow → server handles it (F13.7)
        })
        .collect();

    let mut vt: HashMap<&str, f64> = HashMap::new();
    vt.insert(SERVER_SOURCE_ID, 0.0);
    for (p, _) in &eligible {
        vt.insert(p.id.as_str(), 0.0);
    }

    let mut out = Vec::with_capacity(missing.len());
    for &chunk in missing {
        // Candidates for this chunk: the server (has everything) + peers holding it.
        let mut best_id: &str = SERVER_SOURCE_ID;
        let mut best_vt = vt[SERVER_SOURCE_ID];
        let mut best_weight = cfg.server_weight;
        for (p, w) in &eligible {
            if p.have.contains(&chunk) {
                let v = vt[p.id.as_str()];
                // Strictly-less keeps the server as the tie-break winner.
                if v < best_vt - f64::EPSILON {
                    best_id = p.id.as_str();
                    best_vt = v;
                    best_weight = *w;
                }
            }
        }
        let weight = if best_id == SERVER_SOURCE_ID {
            cfg.server_weight
        } else {
            best_weight
        };
        *vt.get_mut(best_id).unwrap() = best_vt + 1.0 / weight.max(f64::MIN_POSITIVE);
        out.push((chunk, best_id.to_string()));
    }
    out
}

/// Tally assignments by source id (test/diagnostic helper).
pub fn tally(assignments: &[(u64, String)]) -> HashMap<String, usize> {
    let mut m = HashMap::new();
    for (_, id) in assignments {
        *m.entry(id.clone()).or_insert(0) += 1;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_caps_and_refills() {
        let mut tb = TokenBucket::with_burst(1000.0, 1000.0);
        assert!(tb.try_take(1000.0));
        assert!(!tb.try_take(1.0)); // empty
        tb.refill(0.5); // +500
        assert!(tb.try_take(500.0));
        assert!(!tb.try_take(1.0));
        // refill is capped at burst
        tb.refill(100.0);
        assert_eq!(tb.available(), 1000.0);
    }

    #[test]
    fn token_bucket_time_until() {
        let mut tb = TokenBucket::with_burst(1000.0, 1000.0);
        tb.try_take(1000.0);
        assert!((tb.time_until(500.0) - 0.5).abs() < 1e-9);
        let zero = TokenBucket::with_burst(0.0, 0.0);
        assert_eq!(zero.time_until(10.0), f64::INFINITY);
    }

    #[test]
    fn token_bucket_default_burst_fits_a_chunk_and_impossible_is_infinite() {
        // Regression: a low cap (rate < 4 MiB chunk) used to livelock — try_take
        // could never succeed and time_until returned a finite lie. Use an
        // explicit sub-chunk rate (1.5 MiB/s), since the default cap now exceeds
        // the chunk size and wouldn't exercise this.
        let chunk = crate::chunking::DEFAULT_CHUNK_SIZE as f64;
        let mut tb = TokenBucket::new(1_572_864.0);
        assert!(tb.try_take(chunk), "burst must hold one default chunk");
        // and the bucket refills back up to a full chunk eventually
        tb.refill(10.0);
        assert!(tb.try_take(chunk));

        // A request larger than burst can never be satisfied → INFINITY, not
        // a finite wait the caller would spin on.
        let small = TokenBucket::with_burst(1000.0, 1000.0);
        assert_eq!(small.time_until(2000.0), f64::INFINITY);
    }

    #[test]
    fn unmeasured_peers_bootstrap_instead_of_starving() {
        // Regression: a fresh swarm (all peers unmeasured) used to send 100%
        // of chunks to the server forever — measurement could never start.
        let n = 200u64;
        let missing: Vec<u64> = (0..n).collect();
        let peers = vec![PeerSource {
            id: "fresh".into(),
            throughput_bps: None, // never measured
            reachable: true,
            server_only: false,
            have: full_have(n),
        }];
        let a = assign(&missing, &peers, &SchedulerConfig::default());
        let t = tally(&a);
        let fresh = t.get("fresh").copied().unwrap_or(0);
        assert!(fresh > 0, "unmeasured peer must receive bootstrap chunks");
        let server = t.get(SERVER_SOURCE_ID).copied().unwrap_or(0);
        assert!(server > fresh, "server still dominates during bootstrap");
    }

    #[test]
    fn ewma_converges() {
        let mut e = Ewma::new(0.5);
        assert_eq!(e.value(), None);
        e.record(10.0);
        assert_eq!(e.value(), Some(10.0));
        e.record(20.0);
        assert_eq!(e.value(), Some(15.0));
    }

    #[test]
    fn peer_rate_ignores_zero_elapsed() {
        let mut r = PeerRate::default();
        r.record(4 * 1024 * 1024, 0.0); // ignored
        assert_eq!(r.bytes_per_sec(), None);
        r.record(4 * 1024 * 1024, 1.0);
        assert!(r.bytes_per_sec().unwrap() > 0.0);
    }

    fn full_have(n: u64) -> HashSet<u64> {
        (0..n).collect()
    }

    #[test]
    fn faster_peer_gets_more_and_server_dominates() {
        let n = 300u64;
        let missing: Vec<u64> = (0..n).collect();
        let peers = vec![
            PeerSource {
                id: "fast".into(),
                throughput_bps: Some(4.0 * 1024.0 * 1024.0),
                reachable: true,
                server_only: false,
                have: full_have(n),
            },
            PeerSource {
                id: "slow".into(),
                throughput_bps: Some(1.0 * 1024.0 * 1024.0),
                reachable: true,
                server_only: false,
                have: full_have(n),
            },
        ];
        let cfg = SchedulerConfig::default();
        let a = assign(&missing, &peers, &cfg);
        let t = tally(&a);
        let server = t.get(SERVER_SOURCE_ID).copied().unwrap_or(0);
        let fast = t.get("fast").copied().unwrap_or(0);
        let slow = t.get("slow").copied().unwrap_or(0);
        assert_eq!(server + fast + slow, n as usize);
        assert!(
            server > fast,
            "server should take the bulk: {server} vs {fast}"
        );
        assert!(
            fast > slow,
            "fast peer should get more than slow: {fast} vs {slow}"
        );
        assert!(slow > 0, "slow peer should still contribute");
    }

    #[test]
    fn server_only_and_unreachable_and_floor_get_nothing() {
        let n = 50u64;
        let missing: Vec<u64> = (0..n).collect();
        let peers = vec![
            PeerSource {
                id: "isolated".into(),
                throughput_bps: Some(10.0 * 1024.0 * 1024.0),
                reachable: true,
                server_only: true, // self-test failed
                have: full_have(n),
            },
            PeerSource {
                id: "gone".into(),
                throughput_bps: Some(10.0 * 1024.0 * 1024.0),
                reachable: false,
                server_only: false,
                have: full_have(n),
            },
            PeerSource {
                id: "tooslow".into(),
                throughput_bps: Some(1.0), // measured below floor
                reachable: true,
                server_only: false,
                have: full_have(n),
            },
        ];
        let a = assign(&missing, &peers, &SchedulerConfig::default());
        let t = tally(&a);
        assert_eq!(t.get(SERVER_SOURCE_ID).copied().unwrap(), n as usize);
        assert!(!t.contains_key("isolated"));
        assert!(!t.contains_key("gone"));
        assert!(!t.contains_key("tooslow"));
    }

    #[test]
    fn chunk_only_server_has_goes_to_server() {
        let missing = vec![0u64, 1, 2];
        let peers = vec![PeerSource {
            id: "p".into(),
            throughput_bps: Some(10.0 * 1024.0 * 1024.0),
            reachable: true,
            server_only: false,
            have: HashSet::from([0, 1]), // does NOT have chunk 2
        }];
        let a = assign(&missing, &peers, &SchedulerConfig::default());
        let for_2 = a.iter().find(|(c, _)| *c == 2).unwrap();
        assert_eq!(for_2.1, SERVER_SOURCE_ID);
    }
}
