//! Pure-text span diff + 3-way text merge — the dep-clean engine the layered-doc
//! apply paths and (later) the unified conflict surface ride on. No Yrs, no
//! I/O: just text in, spans/merged-text out. `similar` is confined here. See
//! docs/diff.md and docs/sync.md (`sync-three-way-merge`).

use std::time::Duration;

use similar::{Algorithm, DiffTag, TextDiff};

/// Upper bound on the char-level diff a single save may spend computing. A
/// pathologically large rewrite degrades to a coarser (still correct) span
/// rather than stalling the save; the common small-edit case finishes far
/// inside this budget.
const DIFF_TIMEOUT: Duration = Duration::from_secs(1);

/// Byte offset of every char start in `s`, plus a trailing `s.len()`
/// sentinel — so a char index from a char-level diff maps straight to a byte
/// offset (the layered doc records edits in byte positions).
fn char_byte_bounds(s: &str) -> Vec<usize> {
    let mut bounds: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
    bounds.push(s.len());
    bounds
}

/// The minimal set of localized edit spans turning `before` into `after`,
/// each `(byte_start, removed_len, inserted)` in `before`'s coordinate space,
/// ascending and non-overlapping. A char-level Myers diff keeps every span as
/// small as the actual change: a save that touches two distant regions yields
/// two small ops, never one delete-and-reinsert of everything between them.
/// That locality is what keeps the 3-way merge clean — untouched bytes (including
/// a concurrent remote edit elsewhere in the file) are never rewritten, so a
/// remote op against them still merges cleanly.
///
/// `similar` orders its ops by the *new* sequence, so a pure-insert op's
/// `old_range()` anchor is **not** reliably monotonic: an insert that lands
/// after a kept character reports an old anchor *before* the preceding op's
/// old end (a `Delete .. Equal .. Insert` run anchors the insert inside the
/// delete). Mapping each op straight to an `old_range().start` span therefore
/// produces overlapping spans, which `apply_replaces` (high-offset-first
/// remove-then-insert) silently corrupts. We instead track our own `before`
/// cursor, advanced only by ops that consume `before` (`Equal`/`Delete`/
/// `Replace`), and coalesce each maximal run of non-`Equal` ops into one span
/// anchored at that cursor — guaranteeing the ascending, disjoint spans
/// `apply_replaces` requires.
///
/// status: op-log-materialization
pub(crate) fn multi_span_delta(before: &str, after: &str) -> Vec<(usize, usize, String)> {
    if before == after {
        return Vec::new();
    }
    let before_bounds = char_byte_bounds(before);
    let after_bounds = char_byte_bounds(after);
    let started = std::time::Instant::now();
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .timeout(DIFF_TIMEOUT)
        .diff_chars(before, after);
    // `similar` doesn't report that its deadline fired — it just returns
    // coarser ops. Wall-clock is the only available signal, so warn when the
    // budget was plausibly exhausted: the spans this save records are wider
    // than the real edit, and future 3-way merges against them lose locality
    // (disjoint concurrent edits can start surfacing as same-region
    // conflicts). Content stays correct either way.
    if started.elapsed() >= DIFF_TIMEOUT {
        tracing::warn!(
            before_bytes = before.len(),
            after_bytes = after.len(),
            "span diff hit its time budget; recording coarser edit spans \
             for this save (merge locality degraded, content unaffected)"
        );
    }

    let mut spans = Vec::new();
    // The open run, in `before` char indices: `[run_start, run_end)` is the
    // region it removes, `run_inserted` its replacement. `cursor` is how far
    // into `before` we've consumed — the anchor a fresh run starts at.
    let mut run_start: Option<usize> = None;
    let mut run_end = 0usize;
    let mut run_inserted = String::new();
    let mut cursor = 0usize;

    for op in diff.ops() {
        let old = op.old_range();
        if op.tag() == DiffTag::Equal {
            if let Some(start) = run_start.take() {
                let start_b = before_bounds[start];
                spans.push((start_b, before_bounds[run_end] - start_b, std::mem::take(&mut run_inserted)));
            }
            cursor = old.end;
            continue;
        }
        let new = op.new_range();
        if run_start.is_none() {
            run_start = Some(cursor);
            run_end = cursor;
        }
        // Only `Delete`/`Replace` consume `before`; a pure insert is
        // zero-width and leaves `run_end` (and the cursor) where they are.
        if old.end > run_end {
            run_end = old.end;
        }
        run_inserted.push_str(&after[after_bounds[new.start]..after_bounds[new.end]]);
        cursor = run_end;
    }
    if let Some(start) = run_start {
        let start_b = before_bounds[start];
        spans.push((start_b, before_bounds[run_end] - start_b, run_inserted));
    }
    spans
}

/// Three-way text merge for lineage adoption (`sync-lineage-adoption`): combine
/// our divergence (`ours`, the adopting device's local text) and the peer's
/// divergence (`theirs`, the canonical text) over their common ancestor
/// (`base`, the pre-divergence seed) into one merged text.
///
/// Two independently-seeded lineages can't be reconciled by position alone
/// (identical bytes would interleave because the lineages share no history), so
/// the merge happens at
/// the text level over the known common base, then lands as `user` ops on the
/// adopted canonical lineage. The merge replays the peer's text (`theirs` is
/// the new base on disk) and re-applies *our* localized divergence spans
/// (`base → ours`) on top, shifting each span's offset by the net byte delta
/// the peer's earlier edits introduced — so disjoint edits both survive. A span
/// that would land inside a region the peer also changed is dropped (the peer's
/// content wins there), keeping the merge deterministic rather than interleaving
/// a genuine overlap.
///
/// status: op-log-multi-device-sync
pub(crate) fn three_way_merge(base: &str, ours: &str, theirs: &str) -> String {
    // Our localized divergence, in `base` coordinates (ascending, disjoint).
    let our_spans = multi_span_delta(base, ours);
    // The peer's changed regions, in `base` coordinates, so we can both shift
    // our spans past the peer's net length change and detect overlaps.
    let their_spans = multi_span_delta(base, theirs);
    let mut merged = theirs.to_string();
    // Apply our spans high-offset-first so an earlier application never shifts a
    // later span's coordinates (the `apply_replaces` discipline).
    for (start, removed_len, inserted) in our_spans.iter().rev() {
        let our_end = start + removed_len;
        // An EXACT twin on the peer side (same anchor, same removed length, same
        // inserted text) means this edit already converged in `theirs` — the peer
        // applied the identical change (e.g. our own content echoed back via
        // sync, or both sides typed the same first content). Re-applying it would
        // DUPLICATE it: two zero-width inserts of the same text at the same offset
        // are NOT caught by the range-overlap test below (`start < ts+tl` is
        // `0 < 0` for an insertion), so without this skip the span lands a second
        // copy on top of `theirs` (the dirty-buffer-autocommit doubling bug).
        // Mirrors `spans_overlap`'s identical-twin rule. status: op-log-materialization
        if their_spans
            .iter()
            .any(|(ts, tl, tins)| ts == start && tl == removed_len && tins == inserted)
        {
            continue;
        }
        // Drop a span overlapping any region the peer also edited — the peer's
        // content is canonical there (no silent interleave of a real conflict).
        if span_overlaps_any(*start, our_end, &their_spans) {
            continue;
        }
        // Shift by the peer's net byte change strictly *before* this span, so
        // the offset lands in `theirs`/`merged` coordinates.
        let shift: isize = their_spans
            .iter()
            .filter(|(ts, tl, _)| ts + tl <= *start)
            .map(|(_, tl, ins)| ins.len() as isize - *tl as isize)
            .sum();
        let m_start = (*start as isize + shift).clamp(0, merged.len() as isize) as usize;
        let m_end = (m_start + removed_len).min(merged.len());
        if !merged.is_char_boundary(m_start) || !merged.is_char_boundary(m_end) {
            continue;
        }
        merged.replace_range(m_start..m_end, inserted);
    }
    merged
}

/// The merged text plus the byte ranges where `ours` and `theirs` contended,
/// for the unified conflict surface to mark. See [`three_way_merge_regions`].
///
/// status: sync-three-way-merge, sync-unified-conflict-surface
pub(crate) struct MergeOutcome {
    pub merged: String,
    /// Byte ranges in `merged` a dropped-our-span overlapped (peer content won).
    pub conflicts: Vec<std::ops::Range<usize>>,
}

/// One 3-way text merge that also reports the regions where `ours` and `theirs`
/// contended (overlapping divergent spans). `merged` is identical to
/// `three_way_merge(base, ours, theirs)` (peer/theirs wins a contended region —
/// no silent interleave); `conflicts` are the byte ranges IN `merged` that a
/// dropped-our-span overlapped, for the unified conflict surface to mark.
/// status: sync-three-way-merge, sync-unified-conflict-surface
pub(crate) fn three_way_merge_regions(base: &str, ours: &str, theirs: &str) -> MergeOutcome {
    let our_spans = multi_span_delta(base, ours);
    let their_spans = multi_span_delta(base, theirs);
    let mut merged = theirs.to_string();
    // Contended their-regions recorded in `theirs`-coordinates (a STABLE frame
    // that ignores our disjoint splices); mapped into `merged`-coordinates only
    // after every splice has happened. `disjoint_splices` records each disjoint
    // OUR-span actually spliced as `(apply_pos_in_theirs_coords, net_len_delta)`
    // so we can shift a contended region by the splices at/below it.
    let mut contended_theirs: Vec<std::ops::Range<usize>> = Vec::new();
    let mut disjoint_splices: Vec<(usize, isize)> = Vec::new();
    // Apply our spans high-offset-first so an earlier application never shifts a
    // later span's coordinates (mirrors `three_way_merge`).
    for (start, removed_len, inserted) in our_spans.iter().rev() {
        let our_end = start + removed_len;
        // Exact twin already converged in `theirs` — not a conflict, skip.
        if their_spans
            .iter()
            .any(|(ts, tl, tins)| ts == start && tl == removed_len && tins == inserted)
        {
            continue;
        }
        // The peer's net byte change strictly *before* this span — the same
        // shift `three_way_merge` applies to map `base` offsets into `merged`.
        let shift: isize = their_spans
            .iter()
            .filter(|(ts, tl, _)| ts + tl <= *start)
            .map(|(_, tl, ins)| ins.len() as isize - *tl as isize)
            .sum();
        // Contended: our span overlaps a region the peer also edited. The
        // peer's content wins (as in `three_way_merge`, the span is dropped),
        // but we record the overlapping their-span ranges in `theirs`
        // coordinates; they're mapped into `merged` after all splices.
        if span_overlaps_any(*start, our_end, &their_spans) {
            for (ts, tl, tins) in their_spans.iter() {
                // Same half-open overlap predicate `span_overlaps_any` uses,
                // per their-span, so we know which peer region(s) contended.
                if *start < ts + tl && *ts < our_end {
                    // The peer span's inserted text sits in `theirs` at the
                    // base offset `ts` shifted by the net change of all peer
                    // spans strictly before it. This is the stable
                    // `theirs`-coordinate; our disjoint splices are mapped in
                    // afterward.
                    let their_shift: isize = their_spans
                        .iter()
                        .filter(|(ots, otl, _)| ots + otl <= *ts)
                        .map(|(_, otl, oins)| oins.len() as isize - *otl as isize)
                        .sum();
                    let c_start = (*ts as isize + their_shift).max(0) as usize;
                    contended_theirs.push(c_start..(c_start + tins.len()));
                }
            }
            continue;
        }
        let m_start = (*start as isize + shift).clamp(0, merged.len() as isize) as usize;
        let m_end = (m_start + removed_len).min(merged.len());
        if !merged.is_char_boundary(m_start) || !merged.is_char_boundary(m_end) {
            continue;
        }
        // This disjoint OUR-span is spliced into `merged` at `m_start` (a
        // `theirs`-coordinate position). Record its net length delta so we can
        // shift contended regions at/after it.
        disjoint_splices.push((m_start, inserted.len() as isize - *removed_len as isize));
        merged.replace_range(m_start..m_end, inserted);
    }
    // Map each contended their-region from `theirs`-coords to `merged`-coords by
    // adding the summed net delta of every disjoint OUR-splice at or before the
    // region's start. A splice at `m_start <= c_start` shifts the region; a
    // splice strictly after does not (half-open ranges + high-offset-first apply).
    let mut conflicts: Vec<std::ops::Range<usize>> = Vec::new();
    for region in &contended_theirs {
        let delta: isize = disjoint_splices
            .iter()
            .filter(|(m_start, _)| *m_start <= region.start)
            .map(|(_, d)| *d)
            .sum();
        let c_start = (region.start as isize + delta).clamp(0, merged.len() as isize) as usize;
        let c_end = (region.end as isize + delta).clamp(0, merged.len() as isize) as usize;
        if merged.is_char_boundary(c_start) && merged.is_char_boundary(c_end) {
            conflicts.push(c_start..c_end);
        }
    }
    conflicts.sort_by_key(|r| r.start);
    conflicts.dedup();
    MergeOutcome { merged, conflicts }
}

// ===========================================================================
// Unified conflict surface (`sync-unified-conflict-surface`)
//
// One conflict-region model + one set of resolution verbs shared by every
// conflict source: local user-vs-agent overlap (`op-log-merge-conflict`), sync
// same-region contention (`sync-conflict-detect-same-region`), and (git
// transport) external merge markers. A conflicted buffer carries VS-Code-style
// markers VERBATIM in its text; gating + live-preview pass-through key off the
// `has_unresolved_conflicts` predicate over that text.
// ===========================================================================

/// The VS-Code conflict-marker sentinels a conflicted buffer carries verbatim.
/// `<<<<<<<` opens the "ours" half, `=======` separates it from "theirs", and
/// `>>>>>>>` closes the "theirs" half. The labels after the open/close markers
/// (` ours` / ` theirs`) are decorative; detection (`has_unresolved_conflicts`)
/// and parsing key off the seven-char run at line start. A line starting with
/// any of these is reserved — see `render_conflict_markers`.
pub const CONFLICT_MARK_OURS: &str = "<<<<<<<";
pub const CONFLICT_MARK_SEP: &str = "=======";
pub const CONFLICT_MARK_THEIRS: &str = ">>>>>>>";

/// One contended region of a 3-way merge, in the unified model both the inline
/// patch-review overlay and the sync resolution feed: the `ours`/`theirs` text
/// that disagreed, plus (for the merge-derived case) the byte range in the
/// clean `merged` text the region maps to. The same `(range, ours, theirs)`
/// shape `docs/sync.md` "Unified conflict surface" specifies.
///
/// status: sync-unified-conflict-surface
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRegion {
    /// Byte range in the clean (peer-wins) `merged` text this region covers.
    /// `0..0` for a region parsed back out of a marker-bearing buffer, where
    /// there is no separate clean-merged frame to index into.
    pub range: std::ops::Range<usize>,
    /// Our divergent text for the region (the local / user / "mine" side).
    pub ours: String,
    /// The peer's divergent text for the region (the incoming / "theirs" side).
    pub theirs: String,
}

/// The contended regions of a 3-way merge as the unified [`ConflictRegion`]
/// model — each carrying both the `ours` and `theirs` text that disagreed and
/// the byte range in the peer-wins `merged` text it maps to. Both conflict
/// sources (local user-vs-agent overlap and sync same-region) compute their
/// regions through this one path so the inline overlay and the sync resolution
/// share a model. A clean merge (disjoint / fast-forward / identical) yields an
/// empty vec.
///
/// The `range`/`theirs` come straight from [`three_way_merge_regions`] (the
/// peer text wins a contended region, so the recorded range already covers the
/// peer's bytes in `merged`); `ours` is the overlapping divergent text on our
/// side, recovered from our base→ours spans against the same base.
///
/// status: sync-unified-conflict-surface
pub fn conflict_regions(base: &str, ours: &str, theirs: &str) -> Vec<ConflictRegion> {
    let outcome = three_way_merge_regions(base, ours, theirs);
    let our_spans = multi_span_delta(base, ours);
    let their_spans = multi_span_delta(base, theirs);
    outcome
        .conflicts
        .into_iter()
        .map(|range| {
            // The peer text for this region is exactly the merged bytes it
            // covers (peer wins, so the range already names them). Our text is
            // the inserted side of whichever of OUR base-spans overlapped the
            // same base region as the peer span that produced this region — i.e.
            // the divergent content we'd have written there.
            let theirs_text = outcome.merged.get(range.clone()).unwrap_or_default().to_string();
            let ours_text = overlapping_our_text(&our_spans, &their_spans, &theirs_text);
            ConflictRegion { range, ours: ours_text, theirs: theirs_text }
        })
        .collect()
}

/// Recover the "ours" text for a contended region: the inserted text of every
/// OUR base-span that overlaps a THEIR base-span whose inserted text equals the
/// region's peer text. Concatenated in span order. Empty when our side only
/// deleted (no inserted bytes) — a delete-vs-edit region renders an empty ours
/// half, which is the correct VS-Code rendering.
fn overlapping_our_text(
    our_spans: &[(usize, usize, String)],
    their_spans: &[(usize, usize, String)],
    peer_text: &str,
) -> String {
    // The their-span(s) whose inserted text is this region's peer text bound the
    // base region the conflict sits over; collect the base ranges they cover.
    let their_base_ranges: Vec<std::ops::Range<usize>> = their_spans
        .iter()
        .filter(|(_, _, tins)| tins == peer_text)
        .map(|(ts, tl, _)| *ts..(ts + tl))
        .collect();
    let mut out = String::new();
    for (start, removed_len, inserted) in our_spans {
        let end = start + removed_len;
        let overlaps = their_base_ranges
            .iter()
            .any(|r| *start < r.end.max(r.start + 1) && r.start < end.max(*start + 1));
        if overlaps {
            out.push_str(inserted);
        }
    }
    out
}

/// Splice the unified [`ConflictRegion`]s into `merged` as VS-Code-style
/// conflict markers, producing the conflicted-buffer text. Each region's
/// `range` in `merged` (which holds the peer's text — peer wins the clean
/// merge) is replaced by:
///
/// ```text
/// <<<<<<< ours
/// <our text>
/// =======
/// <their text>
/// >>>>>>> theirs
/// ```
///
/// Regions are spliced high-offset-first so an earlier splice never shifts a
/// later region's coordinates. The resulting text satisfies
/// [`has_unresolved_conflicts`] until the user resolves every block. With no
/// regions the input is returned unchanged.
///
/// status: sync-unified-conflict-surface, live-preview-conflict-regions-raw
pub fn render_conflict_markers(merged: &str, regions: &[ConflictRegion]) -> String {
    let mut out = merged.to_string();
    let mut sorted: Vec<&ConflictRegion> = regions.iter().collect();
    sorted.sort_by_key(|r| r.range.start);
    for region in sorted.iter().rev() {
        let (s, e) = (region.range.start.min(out.len()), region.range.end.min(out.len()));
        if !(out.is_char_boundary(s) && out.is_char_boundary(e) && s <= e) {
            continue;
        }
        // Markers are line-oriented: the block must open on its own line and the
        // text that follows the replaced range must resume on a fresh line, even
        // when the conflict range falls mid-line (a same-region word edit). A
        // leading `\n` is prepended unless the splice already sits at a line
        // start; a trailing `\n` ends the block, and the next char gets a fresh
        // line unless it already is one. Otherwise `has_unresolved_conflicts`
        // (which keys off line-start markers) wouldn't see the block.
        let needs_lead = s > 0 && !out[..s].ends_with('\n');
        let next_is_nl = out[e..].starts_with('\n') || e == out.len();
        let lead = if needs_lead { "\n" } else { "" };
        let trail = if next_is_nl { "" } else { "\n" };
        let block = format!(
            "{lead}{CONFLICT_MARK_OURS} ours\n{}\n{CONFLICT_MARK_SEP}\n{}\n{CONFLICT_MARK_THEIRS} theirs{trail}",
            region.ours.trim_end_matches('\n'),
            region.theirs.trim_end_matches('\n'),
        );
        out.replace_range(s..e, &block);
    }
    out
}

/// Whether `text` still holds an unresolved conflict marker — the gate the
/// conflicted-buffer state keys off (`sync-unified-conflict-surface` "Gating").
/// True iff some line starts with the `<<<<<<<` open marker AND a later line
/// starts with the `>>>>>>>` close marker (a complete, still-present block).
/// While true, Save and indexing refuse the buffer until the user resolves it;
/// the markdown live-preview renders the region raw (`live-preview-conflict-
/// regions-raw`). Resolving every block (removing the markers, via the
/// resolution verbs or by hand) flips this back to `false`, re-enabling both.
///
/// status: sync-unified-conflict-surface
pub fn has_unresolved_conflicts(text: &str) -> bool {
    let mut saw_open = false;
    for line in text.lines() {
        if line.starts_with(CONFLICT_MARK_OURS) {
            saw_open = true;
        } else if saw_open && line.starts_with(CONFLICT_MARK_THEIRS) {
            return true;
        }
    }
    false
}

/// Which side of a conflict block the user keeps. The three verbs both the
/// inline patch-review overlay (`op-log-merge-conflict` Keep mine / Keep theirs
/// / Keep both) and the sync resolution (`sync-conflict-resolve-actions`) route
/// through, applied uniformly to the marker-bearing buffer text.
///
/// status: sync-unified-conflict-surface
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepSide {
    /// Keep our text; drop theirs. (Inline "Keep mine".)
    Mine,
    /// Keep their text; drop ours. (Inline "Keep theirs".)
    Theirs,
    /// Keep both, ours first then theirs. (Inline "Keep both".)
    Both,
}

/// Resolve EVERY conflict block in a marker-bearing buffer by `side`, replacing
/// each `<<<<<<< / ======= / >>>>>>>` block with the chosen text and dropping
/// the markers. The verb the unified surface's Keep mine / Keep theirs / Keep
/// both buttons apply; afterwards [`has_unresolved_conflicts`] is `false` and
/// the buffer can save + index again. Text outside conflict blocks is preserved
/// verbatim. A malformed / unterminated block (open with no matching close) is
/// left untouched so a half-typed marker never silently eats content.
///
/// status: sync-unified-conflict-surface
pub fn resolve_all_conflicts(text: &str, side: KeepSide) -> String {
    let mut out = String::new();
    let mut lines = text.lines().peekable();
    let mut first = true;
    // Re-emit a line, preserving the newline shape of the input by tracking
    // whether the original had a trailing newline.
    while let Some(line) = lines.next() {
        if line.starts_with(CONFLICT_MARK_OURS) {
            // Collect ours (until SEP) and theirs (until THEIRS close). If the
            // block never closes, emit the open line verbatim and continue.
            let mut ours: Vec<&str> = Vec::new();
            let mut theirs: Vec<&str> = Vec::new();
            let mut in_theirs = false;
            let mut closed = false;
            let mut block: Vec<&str> = vec![line];
            for inner in lines.by_ref() {
                block.push(inner);
                if inner.starts_with(CONFLICT_MARK_SEP) {
                    in_theirs = true;
                } else if inner.starts_with(CONFLICT_MARK_THEIRS) {
                    closed = true;
                    break;
                } else if in_theirs {
                    theirs.push(inner);
                } else {
                    ours.push(inner);
                }
            }
            if !closed {
                // Unterminated: keep the raw lines untouched.
                for raw in block {
                    push_line(&mut out, &mut first, raw);
                }
                continue;
            }
            let chosen: Vec<&str> = match side {
                KeepSide::Mine => ours,
                KeepSide::Theirs => theirs,
                KeepSide::Both => ours.into_iter().chain(theirs).collect(),
            };
            for kept in chosen {
                push_line(&mut out, &mut first, kept);
            }
        } else {
            push_line(&mut out, &mut first, line);
        }
    }
    if text.ends_with('\n') && !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Append `line` to `out` with a preceding `\n` separator for every line after
/// the first — reconstructing line-joined text without a spurious leading
/// newline.
fn push_line(out: &mut String, first: &mut bool, line: &str) {
    if *first {
        *first = false;
    } else {
        out.push('\n');
    }
    out.push_str(line);
}

/// Whether the half-open byte range `[start, end)` overlaps any span in
/// `spans` (each `(span_start, span_len, _)` in the same coordinate space).
/// The exact predicate [`three_way_merge`] uses to decide a span is a genuine
/// conflict (the peer also edited there) versus a disjoint edit it can shift
/// and keep.
fn span_overlaps_any(start: usize, end: usize, spans: &[(usize, usize, String)]) -> bool {
    spans.iter().any(|(ts, tl, _)| start < ts + tl && *ts < end)
}

/// The minimal single-span edit turning `before` into `after`, as
/// `(byte_start, removed_len, inserted_text)`. Trims the common prefix and
/// suffix and treats the differing middle as one replace — the shape
/// `apply_replace` consumes. For identical inputs returns a zero-length
/// no-op at offset 0. Used by external-edit reconciliation to apply a disk
/// change as one text edit (frontmatter + body share the same `text`, so
/// one delta covers both).
///
/// The split honours UTF-8 char boundaries: the trimmed prefix/suffix are
/// backed off to the nearest boundary so the inserted slice is valid UTF-8.
///
/// status: op-log-external-edit-sync
pub(crate) fn text_delta(before: &str, after: &str) -> (usize, usize, String) {
    let b = before.as_bytes();
    let a = after.as_bytes();
    if b == a {
        return (0, 0, String::new());
    }
    let max_prefix = b.len().min(a.len());
    let mut start = 0;
    while start < max_prefix && b[start] == a[start] {
        start += 1;
    }
    // Back the prefix off to a char boundary in `before` (== same boundary
    // in `after`, since the bytes matched up to `start`).
    while start > 0 && !before.is_char_boundary(start) {
        start -= 1;
    }
    let mut suffix = 0;
    while suffix < (b.len() - start).min(a.len() - start)
        && b[b.len() - 1 - suffix] == a[a.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let before_end = b.len() - suffix;
    let after_end = a.len() - suffix;
    // Back the suffix split off to char boundaries on both sides.
    let mut before_end = before_end;
    while before_end < b.len() && !before.is_char_boundary(before_end) {
        before_end += 1;
    }
    let mut after_end = after_end;
    while after_end < a.len() && !after.is_char_boundary(after_end) {
        after_end += 1;
    }
    let removed_len = before_end.saturating_sub(start);
    let inserted = after[start..after_end.max(start)].to_string();
    (start, removed_len, inserted)
}

#[cfg(test)]
mod delta_tests {
    use super::*;

    /// Apply spans to a String with `apply_replaces`'s high-offset-first
    /// discipline, using plain string ops — an independent oracle for the spans.
    fn apply_spans_str(before: &str, spans: &[(usize, usize, String)]) -> String {
        let mut s = before.to_string();
        for (start, removed_len, inserted) in spans.iter().rev() {
            s.replace_range(*start..(*start + *removed_len), inserted);
        }
        s
    }

    /// Spans must be ascending and non-overlapping in `before` coordinates,
    /// and applying them must reproduce `after`. Regression for the
    /// `similar` new-ordered-ops anchoring bug, where a `Delete .. Equal ..
    /// Insert` run anchored the insert *inside* the delete, yielding
    /// overlapping spans that `apply_replaces` corrupted (chars relocated,
    /// same total length) — e.g. a cluster-tree's `nodes: []` growing into a
    /// `nodes:\n  - id: ...` block dropped the newline after `nodes:`.
    fn assert_delta_sound(before: &str, after: &str) {
        let spans = multi_span_delta(before, after);
        let mut prev_end = 0usize;
        for (start, removed, _) in &spans {
            assert!(*start >= prev_end, "overlapping spans: start {start} < prev_end {prev_end}\n{spans:?}");
            prev_end = start + removed;
        }
        assert_eq!(apply_spans_str(before, &spans), after, "spans do not reproduce `after`");
    }

    #[test]
    fn yaml_seq_growth_anchors_insert_correctly() {
        // The exact shape that broke a cluster-tree confirm: a `nodes: []`
        // frontmatter grows into a populated block sequence. At this size +
        // structure `similar`'s new-ordered ops anchor the insert inside the
        // `Delete " []"` op (a `Delete .. Equal .. Insert` run), the case the
        // old per-op mapping turned into overlapping spans.
        let head = "---\nhiker:\n  kind: cluster-tree\n  id: '01KSG5MBTAEEW98M6BY06TYSFX'\n  name: Semantic\n  source: review:confirm\n  state: draft\n  created_at_ms: 1779732983626\n  nodes:";
        let body = "\n---\n<!-- Cluster tree. The structure lives in the `hiker:` frontmatter above. -->\n";
        let before = format!("{head} []{body}");
        let mut after = String::from(head);
        after.push_str("\n  - id: root\n    kind: cluster\n    name: 'Vault: notes'\n    confidence: 1.0");
        for i in 0..80 {
            after.push_str(&format!(
                "\n  - id: n{i}\n    parent: root\n    kind: leaf\n    note:\n      id: note-{i}\n      path: note-{i}\n    name: 'Cluster #{i}: misc - ascii stuff and things'\n    confidence: 1.0"
            ));
        }
        after.push_str(body);

        // Pure span soundness: the spans are ascending/disjoint and reproduce
        // `after` when applied (the same `apply_spans_str` discipline the commit
        // path runs).
        assert_delta_sound(&before, &after);
    }

    #[test]
    fn assorted_transitions_are_sound() {
        let cases = [
            ("", "hello world"),
            ("hello world", ""),
            ("abc", "aXbYc"),
            ("a: []\nb: 1\n", "a:\n  - x\n  - y\nb: 1\n"),
            ("line\nline\nline\n", "line\nCHANGED\nline\n"),
            ("keep [drop] keep", "keep [\n  insert\n] keep"),
        ];
        for (before, after) in cases {
            assert_delta_sound(before, after);
        }
    }

    /// Disjoint edits on each side both survive; nothing contended.
    #[test]
    fn regions_disjoint_edits_no_conflict() {
        let base = "alpha bravo charlie";
        let ours = "ALPHA bravo charlie"; // edit the head
        let theirs = "alpha bravo CHARLIE"; // edit the tail
        let out = three_way_merge_regions(base, ours, theirs);
        assert_eq!(out.merged, three_way_merge(base, ours, theirs));
        assert!(out.merged.contains("ALPHA"));
        assert!(out.merged.contains("CHARLIE"));
        assert!(out.conflicts.is_empty(), "disjoint edits should not conflict: {:?}", out.conflicts);
    }

    /// Same region, differing edits → peer wins, one conflict range covering
    /// the peer's text there.
    #[test]
    fn regions_same_region_conflict_marks_peer_text() {
        let base = "the quick brown fox";
        let ours = "the slow brown fox";
        let theirs = "the FAST brown fox";
        let out = three_way_merge_regions(base, ours, theirs);
        assert_eq!(out.merged, three_way_merge(base, ours, theirs));
        // Peer wins the contended region.
        assert!(out.merged.contains("FAST"));
        assert!(!out.merged.contains("slow"));
        assert_eq!(out.conflicts.len(), 1, "expected one conflict range: {:?}", out.conflicts);
        let range = out.conflicts[0].clone();
        assert_eq!(&out.merged[range], "FAST", "conflict range should cover the peer's text");
    }

    /// Both sides made the identical edit of base → twin-skip path, no-op merge,
    /// no conflict.
    #[test]
    fn regions_identical_edits_no_conflict() {
        let base = "one two three";
        let edited = "one TWO three";
        let out = three_way_merge_regions(base, edited, edited);
        assert_eq!(out.merged, edited);
        assert!(out.conflicts.is_empty());
    }

    /// `ours == base` → pure fast-forward to `theirs`, no conflict.
    #[test]
    fn regions_fast_forward_no_conflict() {
        let base = "hello world";
        let theirs = "hello brave world";
        let out = three_way_merge_regions(base, base, theirs);
        assert_eq!(out.merged, theirs);
        assert!(out.conflicts.is_empty());
    }

    /// One disjoint edit + one overlapping edit → exactly one conflict, and the
    /// disjoint edit survives in `merged`.
    #[test]
    fn regions_mixed_one_disjoint_one_overlap() {
        let base = "head MIDDLE tail";
        // ours: change the disjoint tail (append X) AND the shared middle.
        let ours = "head ZZZZZZ tailX";
        // theirs: change the shared middle differently (and not the tail).
        let theirs = "head YYYYYY tail";
        let out = three_way_merge_regions(base, ours, theirs);
        assert_eq!(out.merged, three_way_merge(base, ours, theirs));
        // The disjoint tail edit survives.
        assert!(out.merged.contains("tailX"), "disjoint tail edit should survive: {}", out.merged);
        // Peer's middle wins.
        assert!(out.merged.contains("YYYYYY"));
        assert!(!out.merged.contains("ZZZZZZ"), "our contended middle should be dropped: {}", out.merged);
        assert_eq!(out.conflicts.len(), 1, "expected exactly one conflict range: {:?}", out.conflicts);
        assert_eq!(&out.merged[out.conflicts[0].clone()], "YYYYYY");
    }

    /// A disjoint OUR-insert at a LOWER offset than the contended region shifts
    /// that region's real position in `merged`; the recorded conflict range must
    /// track the splice (regression for `theirs`-coordinate ranges pointing at
    /// pre-splice offsets).
    #[test]
    fn regions_disjoint_below_contended_shifts_range() {
        let base = "AAAA BBBB";
        // ours: disjoint insert "xx" at offset 4 AND contended edit BBBB→CCCC.
        let ours = "AAAAxx CCCC";
        // theirs: contended edit BBBB→DDDD (peer wins the contended region).
        let theirs = "AAAA DDDD";
        let out = three_way_merge_regions(base, ours, theirs);
        assert_eq!(out.merged, three_way_merge(base, ours, theirs));
        assert_eq!(out.merged, "AAAAxx DDDD");
        assert_eq!(out.conflicts.len(), 1, "expected exactly one conflict range: {:?}", out.conflicts);
        assert_eq!(
            &out.merged[out.conflicts[0].clone()], "DDDD",
            "conflict range must track the disjoint splice below it"
        );
    }
}

#[cfg(test)]
mod conflict_surface_tests {
    use super::{
        conflict_regions, has_unresolved_conflicts, render_conflict_markers,
        resolve_all_conflicts, KeepSide,
    };

    /// Same-region differing edits → one [`ConflictRegion`] carrying both sides'
    /// divergent text and a range into the clean merged frame covering theirs.
    #[test]
    fn conflict_regions_carry_both_sides() {
        let base = "the quick brown fox";
        let ours = "the slow brown fox";
        let theirs = "the FAST brown fox";
        let regions = conflict_regions(base, ours, theirs);
        assert_eq!(regions.len(), 1, "expected one region: {regions:?}");
        assert_eq!(regions[0].theirs, "FAST");
        assert_eq!(regions[0].ours, "slow");
    }

    /// Disjoint edits → no conflict regions (clean auto-merge).
    #[test]
    fn conflict_regions_empty_on_disjoint() {
        let base = "alpha bravo charlie";
        let ours = "ALPHA bravo charlie";
        let theirs = "alpha bravo CHARLIE";
        assert!(conflict_regions(base, ours, theirs).is_empty());
    }

    /// A fast-forward (`ours == base`) yields no regions.
    #[test]
    fn conflict_regions_empty_on_fast_forward() {
        let base = "hello world";
        let theirs = "hello brave world";
        assert!(conflict_regions(base, base, theirs).is_empty());
    }

    /// Rendering a region produces VS-Code markers the predicate detects, and
    /// the marker text contains both sides verbatim.
    #[test]
    fn render_markers_round_trips_through_predicate() {
        let base = "the quick brown fox";
        let ours = "the slow brown fox";
        let theirs = "the FAST brown fox";
        let merged = super::three_way_merge(base, ours, theirs);
        let regions = conflict_regions(base, ours, theirs);
        let marked = render_conflict_markers(&merged, &regions);
        assert!(has_unresolved_conflicts(&marked), "marked buffer must be conflicted:\n{marked}");
        assert!(marked.contains("<<<<<<< ours"));
        assert!(marked.contains("======="));
        assert!(marked.contains(">>>>>>> theirs"));
        assert!(marked.contains("slow"), "ours side present");
        assert!(marked.contains("FAST"), "theirs side present");
    }

    /// Clean text (no markers) is not conflicted; marker-bearing text is.
    #[test]
    fn predicate_distinguishes_clean_from_conflicted() {
        assert!(!has_unresolved_conflicts("just some text\nwith lines\n"));
        assert!(!has_unresolved_conflicts(""));
        // An open marker without a close is not yet a complete block.
        assert!(!has_unresolved_conflicts("<<<<<<< ours\nmine\n"));
        let block = "intro\n<<<<<<< ours\nmine\n=======\ntheirs\n>>>>>>> theirs\ntail\n";
        assert!(has_unresolved_conflicts(block));
    }

    /// Keep mine drops the markers and keeps the ours half; resolving clears the
    /// conflicted state.
    #[test]
    fn resolve_keep_mine() {
        let block = "head\n<<<<<<< ours\nMINE\n=======\nTHEIRS\n>>>>>>> theirs\ntail\n";
        let out = resolve_all_conflicts(block, KeepSide::Mine);
        assert_eq!(out, "head\nMINE\ntail\n");
        assert!(!has_unresolved_conflicts(&out));
    }

    /// Keep theirs keeps the theirs half.
    #[test]
    fn resolve_keep_theirs() {
        let block = "head\n<<<<<<< ours\nMINE\n=======\nTHEIRS\n>>>>>>> theirs\ntail\n";
        let out = resolve_all_conflicts(block, KeepSide::Theirs);
        assert_eq!(out, "head\nTHEIRS\ntail\n");
        assert!(!has_unresolved_conflicts(&out));
    }

    /// Keep both keeps ours then theirs, dropping only the markers.
    #[test]
    fn resolve_keep_both() {
        let block = "head\n<<<<<<< ours\nMINE\n=======\nTHEIRS\n>>>>>>> theirs\ntail\n";
        let out = resolve_all_conflicts(block, KeepSide::Both);
        assert_eq!(out, "head\nMINE\nTHEIRS\ntail\n");
        assert!(!has_unresolved_conflicts(&out));
    }

    /// Multiple conflict blocks all resolve in one pass.
    #[test]
    fn resolve_multiple_blocks() {
        let block = "<<<<<<< ours\nA1\n=======\nB1\n>>>>>>> theirs\nmid\n<<<<<<< ours\nA2\n=======\nB2\n>>>>>>> theirs\n";
        let out = resolve_all_conflicts(block, KeepSide::Mine);
        assert_eq!(out, "A1\nmid\nA2\n");
        assert!(!has_unresolved_conflicts(&out));
    }

    /// An unterminated block is left untouched rather than eating content.
    #[test]
    fn resolve_leaves_unterminated_block() {
        let block = "head\n<<<<<<< ours\nMINE\nno close marker here\n";
        let out = resolve_all_conflicts(block, KeepSide::Mine);
        assert_eq!(out, block);
    }
}
