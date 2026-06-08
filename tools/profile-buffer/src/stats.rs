//! Per-rebuild timing statistics, reported in microseconds (mirrors
//! `tools/profile-canvas`'s `Stats`). One sample per timed caret-move rebuild.

use std::time::Duration;

/// Percentile / max rebuild-time summary in microseconds.
pub struct Stats {
    /// 50th-percentile (median) rebuild time.
    pub p50: u128,
    /// 95th-percentile rebuild time.
    pub p95: u128,
    /// Slowest observed rebuild.
    pub max: u128,
}

impl Stats {
    /// Sort `samples` in place and reduce them to p50 / p95 / max in micros.
    pub fn summarize(samples: &mut [Duration]) -> Self {
        samples.sort_unstable();
        let n = samples.len();
        let pct = |p: usize| samples[((n * p) / 100).min(n.saturating_sub(1))].as_micros();
        Self {
            p50: pct(50),
            p95: pct(95),
            max: samples.last().map_or(0, Duration::as_micros),
        }
    }
}
