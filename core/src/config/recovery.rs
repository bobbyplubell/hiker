//! Local-history config: the `[history]` section. Split from `sections.rs`
//! (the `vcs.rs` precedent) because plain-file snapshots are a cohesive,
//! self-contained concept (paired with `core::snapshot`) and `sections.rs` had
//! grown past its file-length budget. The parent module re-exports this
//! alongside the other section types.

use serde::{Deserialize, Serialize};

/// `[history]` section. Retention policy for plain-file note snapshots
/// (`core::snapshot`) — the Obsidian-style "File Recovery" whole-file history
/// kept under `<vault>/.hiker/history/`. Each save writes one `.md` snapshot;
/// the set is then pruned to keep at most `max_snapshots` (newest wins) AND
/// drop anything older than `max_age_days`. Snapshots are disposable cache —
/// `rm -rf .hiker/history` loses nothing canonical. `0` on either knob disables
/// that dimension of pruning. User + vault scope.
///
/// status: plain-file-snapshots
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryConfig {
    /// Maximum snapshots retained per note (newest wins). `0` = unbounded.
    #[serde(default = "default_max_snapshots")]
    pub max_snapshots: u32,
    /// Age (days) past which a snapshot is pruned. `0` = never age out.
    #[serde(default = "default_max_age_days")]
    pub max_age_days: u32,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            max_snapshots: default_max_snapshots(),
            max_age_days: default_max_age_days(),
        }
    }
}

impl From<&HistoryConfig> for crate::snapshot::RetentionPolicy {
    fn from(cfg: &HistoryConfig) -> Self {
        Self {
            max_snapshots: cfg.max_snapshots,
            max_age_days: cfg.max_age_days,
        }
    }
}

const fn default_max_snapshots() -> u32 { crate::snapshot::DEFAULT_MAX_SNAPSHOTS }
const fn default_max_age_days() -> u32 { crate::snapshot::DEFAULT_MAX_AGE_DAYS }
