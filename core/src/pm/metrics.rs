//! Sprint metrics derived by replaying the board-doc's plain-file snapshot
//! history (`core::snapshot`, `docs/pm.md` §"Metrics").
//!
//! Every card move is a frontmatter edit on the board-doc, and every save
//! writes a whole-file snapshot under `.hiker/history/<rel>/`, so the
//! sprint's snapshot set *is* the tracking data: walk
//! `op_writes::snapshot_history`, read each snapshot via
//! `content_at_snapshot`, parse it with the board parser, and diff
//! consecutive column memberships into transition events; map columns to
//! categories through the kind's column-state map; join `estimate` per card
//! from the metadata index. Nothing here writes.
//!
//! Honest bounds, stated up front:
//!
//! - **Debounce coalescing.** Snapshots are per-save and debounce-coalesced,
//!   so burst moves collapse into one transition — fine for the
//!   day-granularity charts these tables feed.
//! - **Retroactive estimate joins.** Estimates are joined at *computation*
//!   time from the current index, so editing an estimate re-weights past
//!   charts. Accepted: a point-in-time join would require replaying every
//!   member note's history too.
//! - **Retention-bounded replay.** Snapshots are pruned by count/age
//!   (`[history]` config), so old sprints' charts degrade to whatever
//!   snapshots survive — counted as `skipped_unretained` is no longer
//!   meaningful per-frame (a pruned snapshot simply doesn't appear), but
//!   the truncation marker still fires when a snapshot won't parse.
//!   TODO(K3c): reduced historical fidelity when snapshots are pruned or a
//!   vault predates snapshotting is accepted per the core-rework plan.
//
// status: pm-snapshot-metrics

use std::collections::BTreeMap;

use crate::boards::{parse_board_for, Board, BoardCard};
use crate::errors::HikerError;
use crate::frontmatter::iso_date_epoch;
use crate::kinds::{Registry, StateCategory};
use crate::editing::LayeredDoc;
use crate::ops::op_writes;
use crate::store::Store;

use super::{estimate_of, owning_plan};

/// Borrow-bundle of the read-only handles every metrics builder consumes.
pub struct Ctx<'a> {
    pub log: &'a LayeredDoc,
    pub store: &'a Store,
    pub registry: &'a Registry,
}

/// One day of the burnup series: cumulative count + `estimate` sum of
/// cards in `done`-category columns as of that day's last frame, against
/// the total on the sprint at that frame.
#[derive(Debug, Clone, PartialEq)]
pub struct BurnupRow {
    /// UTC calendar day, `YYYY-MM-DD`.
    pub day: String,
    pub done_count: usize,
    pub done_estimate: f64,
    pub total_count: usize,
    pub total_estimate: f64,
}

/// One completed cycle for a card: entry into an `in_progress`-category
/// column to its next entry into a `done`-category column. A card that is
/// reopened (done -> in_progress -> done) emits one row per completed
/// cycle; a card that never completed both legs emits no row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleRow {
    /// The card's handle: a note card's vault path or a freeform card id.
    pub handle: String,
    pub started_ms: i64,
    pub done_ms: i64,
}

impl CycleRow {
    /// Elapsed cycle time in fractional days — the distribution charts bin.
    #[must_use]
    pub fn days(&self) -> f64 {
        (self.done_ms - self.started_ms) as f64 / 86_400_000.0
    }
}

/// One closed sprint's velocity: the `estimate` sum (and count) of
/// `done`-category cards at its close frame.
#[derive(Debug, Clone, PartialEq)]
pub struct VelocityRow {
    pub sprint_rel: String,
    pub done_count: usize,
    pub done_estimate: f64,
}

/// The three derived tables for one sprint board, plus the newest snapshot
/// id (informational — the most recent history frame the replay saw; the
/// render strip memoizes on [`inputs_fingerprint`], NOT this), plus the
/// honesty counters: how many of THIS board's history rows the replay could
/// not use. Nonzero counts mean the charts tell a truncated story, and the
/// render surface says so instead of silently charting the gap as zeros.
#[derive(Debug, Clone, Default)]
pub struct SprintTables {
    /// The newest retained snapshot id seen during replay (informational).
    pub newest_op_id: Option<String>,
    pub burnup: Vec<BurnupRow>,
    pub cycle: Vec<CycleRow>,
    pub velocity: Vec<VelocityRow>,
    /// History rows whose frame fell outside retention
    /// (`content_at_op` -> `None`) during this board's replay.
    pub skipped_unretained: usize,
    /// History rows whose materialized text didn't parse as a board-doc
    /// (mid-edit saves, lifecycle markers).
    pub skipped_unparseable: usize,
}

impl SprintTables {
    /// Total frames the replay skipped — the strip's truncation marker
    /// renders when this is nonzero.
    #[must_use]
    pub const fn skipped_frames(&self) -> usize {
        self.skipped_unretained + self.skipped_unparseable
    }
}

/// One materialized + parsed history frame of a board-doc.
struct Frame {
    at_ms: i64,
    board: Board,
}

/// A board-doc's replayed frames plus the per-kind skip counts the replay
/// observed — the raw output [`SprintTables`] surfaces.
struct Replay {
    frames: Vec<Frame>,
    skipped_unretained: usize,
    skipped_unparseable: usize,
}

impl Replay {
    const fn skipped(&self) -> usize {
        self.skipped_unretained + self.skipped_unparseable
    }
}

/// Cap on the burnup day span, guarding against pathological
/// `start`/`end` frontmatter (a typo'd year would otherwise emit
/// millions of rows).
const MAX_BURNUP_DAYS: i64 = 1000;

/// History fetch ceiling for the replay. Effectively "everything" —
/// retention bounds real histories far below this — while staying inside
/// the layered doc's `LIMIT` integer range (`usize::MAX` is not a valid SQLite
/// limit).
const MAX_REPLAY_FRAMES: usize = 100_000;

/// A cheap fingerprint of EVERY input the metrics tables read, for memoizing
/// the rendered strip. Unlike [`sprint_tables`] this does no historical
/// snapshot replay (no per-frame file reads) — it captures only the *current*
/// state that drives the tables:
///
/// - the board-doc's current accepted content (structure / columns / cards) —
///   so a board edit, dedup'd or not, changes the fingerprint;
/// - each current note card's `estimate` meta — so the burnup/velocity
///   estimate join invalidates even though changing an estimate writes
///   `note_meta`, NOT a board snapshot (the case the old snapshot-id key
///   silently missed);
/// - the owning plan's sprints and their `closed_at` stamps — so a sprint
///   close re-derives velocity.
///
/// Returns `None` only when the board has no doc to read; the strip treats
/// that as "nothing to memoize" and recomputes (cheap, empty result).
pub fn inputs_fingerprint(ctx: &Ctx<'_>, board_rel: &str) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let accepted = ctx.log.materialize_accepted(board_rel).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    accepted.text.hash(&mut hasher);
    // Current note cards' estimates (sorted for determinism).
    if let Ok(board) = parse_board_for(board_rel, &accepted.text, Some(ctx.registry)) {
        let mut card_estimates: Vec<(String, u64)> = board
            .columns
            .iter()
            .flat_map(|c| &c.cards)
            .filter_map(BoardCard::path)
            .map(|p| (p.to_string(), estimate_of(ctx.store, p).to_bits()))
            .collect();
        card_estimates.sort();
        card_estimates.dedup();
        card_estimates.hash(&mut hasher);
    }
    // Plan sprints + their close stamps (velocity inputs).
    let sprints = match owning_plan(ctx.store, ctx.registry, board_rel) {
        Ok(Some(plan)) => ctx
            .store
            .members_of(&plan)
            .map(|ms| ms.into_iter().map(|m| m.member_path).collect())
            .unwrap_or_else(|_| vec![board_rel.to_string()]),
        _ => vec![board_rel.to_string()],
    };
    let mut closes: Vec<(String, Option<String>)> = sprints
        .into_iter()
        .map(|rel| {
            let closed = ctx.store.meta_value(&rel, "closed_at").ok().flatten();
            (rel, closed)
        })
        .collect();
    closes.sort();
    closes.hash(&mut hasher);
    Some(format!("{:016x}", hasher.finish()))
}

/// Compute the full metrics bundle for `board_rel` by replaying its
/// retained history frames. Read-only over the log + index; zero writes.
///
/// status: pm-layered-metrics
pub fn sprint_tables(
    ctx: &Ctx<'_>,
    board_rel: &str,
) -> Result<SprintTables, HikerError> {
    let newest_op_id = op_writes::snapshot_history(ctx.log, board_rel, 1)?
        .first()
        .map(|op| op.snapshot_id.clone());
    let replay = replay_frames(ctx.log, ctx.registry, board_rel)?;
    Ok(SprintTables {
        newest_op_id,
        burnup: burnup(ctx, board_rel, &replay),
        cycle: cycle_times(ctx.registry, &replay.frames),
        velocity: velocity(ctx, board_rel)?,
        skipped_unretained: replay.skipped_unretained,
        skipped_unparseable: replay.skipped_unparseable,
    })
}

/// Materialize + parse every retained accepted frame of `rel`,
/// oldest-first. Best-effort per frame: pre-retention frames
/// (`content_at_op` → `None`) and unparseable frames (mid-edit, lifecycle
/// markers) are skipped — and COUNTED, so the caller can surface the
/// truncation instead of charting the gap as silent zeros.
fn replay_frames(
    log: &LayeredDoc,
    registry: &Registry,
    rel: &str,
) -> Result<Replay, HikerError> {
    let mut history = op_writes::snapshot_history(log, rel, MAX_REPLAY_FRAMES)?;
    history.reverse(); // newest-first -> oldest-first
    let mut replay = Replay {
        frames: Vec::new(),
        skipped_unretained: 0,
        skipped_unparseable: 0,
    };
    for snap in history {
        let Some(text) = op_writes::content_at_snapshot(log, rel, &snap.snapshot_id)? else {
            replay.skipped_unretained += 1;
            continue;
        };
        let Ok(board) = parse_board_for(rel, &text, Some(registry)) else {
            replay.skipped_unparseable += 1;
            continue;
        };
        replay.frames.push(Frame { at_ms: snap.timestamp_ms, board });
    }
    Ok(replay)
}

/// A frame's column membership: card handle (note path / freeform card
/// id) -> column name.
fn membership(board: &Board) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for col in &board.columns {
        for card in &col.cards {
            let handle = match card {
                BoardCard::Note { path } => path.clone(),
                BoardCard::Text { card_id, .. } => card_id.clone(),
            };
            map.insert(handle, col.name.clone());
        }
    }
    map
}

/// The category anchor of `column` on `board`, through the board's kind's
/// column-state map. `None` for unmapped lanes and plain boards.
fn column_category(registry: &Registry, board: &Board, column: &str) -> Option<StateCategory> {
    let kind = registry.board_like(&board.kind)?;
    kind.state_category(kind.columns.get(column)?)
}

/// The burnup series (`pm-layered-metrics`): one row per UTC day in the
/// sprint's `start`..`end` window (frame-timestamp range when the dates
/// are unset), each row the done-vs-total tally as of that day's last
/// retained frame. Days before the first frame tally zero — except under
/// a truncated replay, where the series starts at the first retained
/// frame instead (zero-filling days we can't see would be a lie; the
/// strip's truncation marker names the gap).
fn burnup(ctx: &Ctx<'_>, board_rel: &str, replay: &Replay) -> Vec<BurnupRow> {
    let frames = &replay.frames;
    let truncated = replay.skipped() > 0;
    let Some((first_day, last_day)) = burnup_window(ctx.store, board_rel, frames, truncated)
    else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for day in first_day..=last_day {
        let day_end_ms = (day + 1) * 86_400_000 - 1;
        let frame = frames.iter().rev().find(|f| f.at_ms <= day_end_ms);
        let mut row = BurnupRow {
            day: day_string(day),
            done_count: 0,
            done_estimate: 0.0,
            total_count: 0,
            total_estimate: 0.0,
        };
        if let Some(frame) = frame {
            tally_frame(ctx, frame, &mut row);
        }
        rows.push(row);
    }
    rows
}

/// Tally one frame into a burnup row: every card counts toward the total;
/// cards in `done`-category columns count as done. Estimates join from the
/// current index (freeform cards carry none).
fn tally_frame(ctx: &Ctx<'_>, frame: &Frame, row: &mut BurnupRow) {
    for col in &frame.board.columns {
        let done = column_category(ctx.registry, &frame.board, &col.name)
            == Some(StateCategory::Done);
        for card in &col.cards {
            let estimate = card
                .path()
                .map_or(0.0, |path| estimate_of(ctx.store, path));
            row.total_count += 1;
            row.total_estimate += estimate;
            if done {
                row.done_count += 1;
                row.done_estimate += estimate;
            }
        }
    }
}

/// The inclusive UTC day-index window burnup iterates: the sprint's
/// `start`..`end` kind fields when set (off the metadata index), else the
/// retained frames' timestamp range. Under a `truncated` replay the start
/// is clamped to the first retained frame's day — pre-retention days
/// would otherwise zero-fill as confidently as real data. `None` when
/// there is nothing to chart. Spans are capped at [`MAX_BURNUP_DAYS`].
fn burnup_window(
    store: &Store,
    board_rel: &str,
    frames: &[Frame],
    truncated: bool,
) -> Option<(i64, i64)> {
    let meta_day = |key: &str| -> Option<i64> {
        let value = store.meta_value(board_rel, key).ok().flatten()?;
        Some(iso_date_epoch(&value)? as i64 / 86_400)
    };
    let mut first = meta_day("start").or_else(|| frames.first().map(|f| f.at_ms / 86_400_000))?;
    if truncated && let Some(frame_first) = frames.first().map(|f| f.at_ms / 86_400_000) {
        first = first.max(frame_first);
    }
    let last = meta_day("end")
        .or_else(|| frames.last().map(|f| f.at_ms / 86_400_000))?
        .max(first);
    Some((first, last.min(first + MAX_BURNUP_DAYS - 1)))
}

/// Format a UTC day index (days since the epoch) as `YYYY-MM-DD`.
fn day_string(day: i64) -> String {
    let fmt = time::macros::format_description!("[year]-[month]-[day]");
    time::OffsetDateTime::from_unix_timestamp(day * 86_400)
        .ok()
        .and_then(|odt| odt.date().format(fmt).ok())
        .unwrap_or_else(|| day.to_string())
}

/// Per-card cycle times from the frame diff: a card "enters" a column at
/// the first frame that shows it there (its appearance in the first
/// retained frame counts as an entry — the retention caveat above).
fn cycle_times(registry: &Registry, frames: &[Frame]) -> Vec<CycleRow> {
    let mut started: BTreeMap<String, i64> = BTreeMap::new();
    // One card can complete more than one cycle (done -> reopened ->
    // in-progress -> done), so collect every completed cycle rather than
    // keying a single row per handle.
    let mut rows: Vec<CycleRow> = Vec::new();
    let mut prev: BTreeMap<String, String> = BTreeMap::new();
    for frame in frames {
        let current = membership(&frame.board);
        for (handle, column) in &current {
            if prev.get(handle) == Some(column) {
                continue;
            }
            match column_category(registry, &frame.board, column) {
                Some(StateCategory::InProgress) => {
                    started.entry(handle.clone()).or_insert(frame.at_ms);
                }
                Some(StateCategory::Done) => {
                    // Complete the open cycle and clear `started` so a
                    // subsequent reopen (done -> in_progress -> done)
                    // records a fresh second cycle instead of being lost.
                    if let Some(started_ms) = started.remove(handle) {
                        rows.push(CycleRow {
                            handle: handle.clone(),
                            started_ms,
                            done_ms: frame.at_ms,
                        });
                    }
                }
                _ => {}
            }
        }
        prev = current;
    }
    rows
}

/// The velocity table: one row per *closed* sprint (those with a
/// `closed_at` stamp) — across the owning plan's sprints when the board
/// belongs to a plan (`plan-kind`), else this sprint alone. Each row is
/// the done-category tally at the sprint's close frame: the first frame
/// at or after the `closed_at` instant (the accepted close batch), else
/// the last retained frame.
fn velocity(ctx: &Ctx<'_>, board_rel: &str) -> Result<Vec<VelocityRow>, HikerError> {
    let sprints: Vec<String> = match owning_plan(ctx.store, ctx.registry, board_rel)? {
        Some(plan) => ctx
            .store
            .members_of(&plan)
            .map_err(|e| HikerError::Io(e.to_string()))?
            .into_iter()
            .map(|m| m.member_path)
            .collect(),
        None => vec![board_rel.to_string()],
    };
    let mut rows = Vec::new();
    for rel in sprints {
        let Some(closed_at) = ctx
            .store
            .meta_value(&rel, "closed_at")
            .map_err(|e| HikerError::Io(e.to_string()))?
        else {
            continue;
        };
        let frames = replay_frames(ctx.log, ctx.registry, &rel)?.frames;
        let Some(frame) = close_frame(&frames, &rel, &closed_at) else {
            continue;
        };
        let mut tally = BurnupRow {
            day: String::new(),
            done_count: 0,
            done_estimate: 0.0,
            total_count: 0,
            total_estimate: 0.0,
        };
        tally_frame(ctx, frame, &mut tally);
        rows.push(VelocityRow {
            sprint_rel: rel,
            done_count: tally.done_count,
            done_estimate: tally.done_estimate,
        });
    }
    Ok(rows)
}

/// The close frame of a closed sprint: the first frame at or after the
/// `closed_at` instant — the accepted close batch, which both stamps the
/// marker and removes the rolled-over cards — falling back to the last
/// retained frame when `closed_at` is unparseable or newer than every
/// frame. The fallback is degraded data (the tally may predate the real
/// close), so it warns naming the sprint rather than passing silently.
fn close_frame<'f>(frames: &'f [Frame], sprint_rel: &str, closed_at: &str) -> Option<&'f Frame> {
    // Strip fractional seconds (older stamps carry nanos) — the date
    // mirror parses whole-second RFC3339 only.
    let whole = closed_at.split('.').next().unwrap_or(closed_at);
    let closed_ms = iso_date_epoch(whole).map(|secs| (secs * 1000.0) as i64);
    let hit = closed_ms.and_then(|ms| frames.iter().find(|f| f.at_ms >= ms));
    if hit.is_none() {
        tracing::warn!(
            sprint = sprint_rel,
            closed_at,
            "pm metrics: closed_at unparseable or newer than every retained frame; \
             velocity falls back to the last retained frame"
        );
    }
    hit.or_else(|| frames.last())
}

#[cfg(test)]
mod tests;
