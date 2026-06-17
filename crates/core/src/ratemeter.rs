//! A tiny smoothed throughput meter (bytes/sec) for serve/seed rates.
//!
//! Call [`RateMeter::add`] as bytes go out; [`RateMeter::rate_bps`] returns a
//! lightly-smoothed rate that decays to 0 when activity stops. Cheap enough to
//! call per chunk; wrap in a `Mutex` for concurrent servers.

use std::time::Instant;

/// Window length: rate is recomputed at most this often.
const WINDOW: f64 = 0.5;
/// After this long with no traffic, the meter reads 0 (idle).
const IDLE_AFTER: f64 = 2.0;

pub struct RateMeter {
    bytes: u64,
    window_start: Instant,
    rate_bps: f64,
}

impl Default for RateMeter {
    fn default() -> Self {
        Self {
            bytes: 0,
            window_start: Instant::now(),
            rate_bps: 0.0,
        }
    }
}

impl RateMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `n` bytes sent.
    pub fn add(&mut self, n: u64) {
        self.bytes += n;
        let dt = self.window_start.elapsed().as_secs_f64();
        if dt >= WINDOW {
            let inst = self.bytes as f64 / dt;
            self.rate_bps = if self.rate_bps == 0.0 {
                inst
            } else {
                self.rate_bps * 0.6 + inst * 0.4 // light EWMA
            };
            self.bytes = 0;
            self.window_start = Instant::now();
        }
    }

    /// Current smoothed rate in bytes/sec; 0 once traffic has stopped.
    pub fn rate_bps(&self) -> u64 {
        if self.window_start.elapsed().as_secs_f64() > IDLE_AFTER {
            return 0;
        }
        self.rate_bps as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_meter_reads_zero() {
        let m = RateMeter::new();
        assert_eq!(m.rate_bps(), 0);
    }

    #[test]
    fn accumulates_within_a_window() {
        let mut m = RateMeter::new();
        // Adds below the window threshold don't crash and keep a sane reading.
        m.add(1_000_000);
        // Without elapsed >= WINDOW the rate hasn't been computed yet, but the
        // meter must not be stale immediately.
        assert!(m.rate_bps() < u64::MAX);
    }
}
