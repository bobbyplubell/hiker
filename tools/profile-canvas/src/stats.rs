//! Per-frame timing statistics, reported in microseconds (mirrors
//! `profile-scroll`'s `Stats`).

use std::time::Duration;

/// Percentile / max frame-time summary in microseconds.
pub struct Stats {
    /// 50th-percentile (median) frame time.
    pub p50: u128,
    /// 95th-percentile frame time.
    pub p95: u128,
    /// Slowest observed frame.
    pub max: u128,
}

impl Stats {
    /// Sort `samples` in place and reduce them to p50 / p95 / max in micros.
    pub fn summarize(samples: &mut [Duration]) -> Self {
        samples.sort_unstable();
        let n = samples.len();
        let pct = |p: usize| samples[((n * p) / 100).min(n - 1)].as_micros();
        Self {
            p50: pct(50),
            p95: pct(95),
            max: samples.last().map_or(0, Duration::as_micros),
        }
    }
}
