//! Dirty-diff gutter (`git-dirty-diff-gutter`, G4): per-line change markers in
//! the editor gutter showing how the open buffer differs from its committed
//! state at git HEAD (the VSCode "dirty diff"). The counterpart to the
//! disk-relative `index_diff` layer in [`super::decorations`] — same diff →
//! gutter-marker shape, but the *before* side is `git show HEAD:<path>` instead
//! of the last disk read.
//!
//! ## Split of responsibilities (and the editor boundary)
//!
//! The **pure** part lives here and is unit-tested: [`line_markers`] maps a
//! `(HEAD text, working text)` pair to a `Vec<`[`LineMarker`]`>` — a 0-based
//! after-line index plus an [`Added`](DirtyKind::Added) /
//! [`Modified`](DirtyKind::Modified) / [`Deleted`](DirtyKind::Deleted) kind.
//! This is the load-bearing logic and has no egui / editor-state dependency.
//!
//! [`marker_decorations`] turns those markers into an
//! `editor_core::decoration::Set` carrying `GutterMarker::Diff*` on each line,
//! anchored to the live doc's byte ranges (reusing the exact anchoring the
//! `index_diff` layer uses).
//!
//! **Rendering boundary:** the egui editor widget's `paint_gutter`
//! (`editor/editor-egui/src/widget.rs`) currently paints only the line number
//! and the fold chevron — it does **not** read `LineStyle.gutter_marker`. So
//! these decorations are *computed and wired* but not yet *painted* as a thin
//! VSCode-style strip; making them visible requires an `editor-egui` change
//! (see the module-level note in `decorations.rs` / the G4 report). The mapping
//! and wiring are landed app-side regardless so the visual is a one-function
//! change in the submodule when its owner takes it on.

use editor_core::decoration::Decoration;
use editor_core::decoration::GutterMarker;
use editor_core::decoration::LineStyle;
use editor_core::decoration::Set;
use editor_core::diff::HunkKind;
use editor_core::rangeset::RangeSet;

/// The kind of change a dirty-diff gutter marker represents, mirroring the
/// three VSCode dirty-diff states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirtyKind {
    /// A line present in the working text with no counterpart at HEAD
    /// (green strip).
    Added,
    /// A line that replaced a HEAD line in place (blue strip).
    Modified,
    /// One or more HEAD lines deleted; anchored onto the surviving working
    /// line that now sits where they were (a deletion caret / triangle).
    Deleted,
}

/// A single gutter marker: the 0-based working-text line it sits on plus its
/// kind. `line` indexes into the *working* (right / after) side, so a deletion
/// is anchored onto the surviving line that follows the removed run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineMarker {
    pub line: u32,
    pub kind: DirtyKind,
}

/// Map `(head, working)` to the per-line dirty-diff markers — the pure,
/// unit-tested core of the dirty-diff gutter.
///
/// Strategy (identical to the disk-relative `index_diff` layer so the two
/// gutters read the same): run the line-level diff, then for each hunk:
/// - `Added` right lines → [`DirtyKind::Added`];
/// - `Modified` right lines → [`DirtyKind::Modified`];
/// - `Removed` runs have no after-line of their own, so they collapse onto the
///   next surviving after-line as [`DirtyKind::Deleted`] (or, at EOF, onto the
///   last surviving line).
///
/// Returns markers sorted by line, at most one per line (an Add/Modify already
/// on a line wins over a later Delete collapsing onto it — matching the legacy
/// gutter, where the change marker is louder than the adjacent deletion caret).
/// An empty result means the working text is byte-identical to HEAD.
pub fn line_markers(head: &str, working: &str) -> Vec<LineMarker> {
    if head == working {
        return Vec::new();
    }
    let right_line_count = working.lines().count();
    let hunks = editor_core::diff::lines(head, working);
    // 0-based after-line -> kind. BTreeMap keeps the output sorted; `entry`
    // semantics give an existing Add/Modify priority over a colliding Delete.
    let mut per_line: std::collections::BTreeMap<u32, DirtyKind> =
        std::collections::BTreeMap::new();
    // Highest 1-based after-line index seen on a surviving (context/add/modify)
    // hunk, used to anchor an end-of-file deletion onto the last real line.
    let mut last_after_seen: u32 = 0;
    for hunk in &hunks {
        match hunk.kind {
            HunkKind::Context => {
                last_after_seen = last_after_seen.max(hunk.right_lines.end as u32);
            }
            HunkKind::Added => {
                for r in hunk.right_lines.clone() {
                    per_line.insert(r as u32, DirtyKind::Added);
                    last_after_seen = last_after_seen.max(r as u32 + 1);
                }
            }
            HunkKind::Modified => {
                for r in hunk.right_lines.clone() {
                    per_line.insert(r as u32, DirtyKind::Modified);
                    last_after_seen = last_after_seen.max(r as u32 + 1);
                }
            }
            HunkKind::Removed => {
                // Collapse onto the next surviving after-line; if the deletion
                // is at EOF, onto the last surviving after-line. `or_insert`
                // means an Add/Modify already there keeps priority.
                if hunk.right_lines.start < right_line_count {
                    per_line
                        .entry(hunk.right_lines.start as u32)
                        .or_insert(DirtyKind::Deleted);
                } else if last_after_seen > 0 {
                    per_line
                        .entry(last_after_seen.saturating_sub(1))
                        .or_insert(DirtyKind::Deleted);
                }
            }
        }
    }
    per_line
        .into_iter()
        .map(|(line, kind)| LineMarker { line, kind })
        .collect()
}

/// Convert dirty-diff [`LineMarker`]s into a decoration [`Set`] that places a
/// `GutterMarker::Diff*` on each marked line of `doc`, anchored to the live
/// doc's byte ranges. The byte-range anchoring matches the `index_diff` layer
/// (empty lines get a 1-byte range so the line still renders a marker).
///
/// `markers` is expected to come from [`line_markers`] over the same working
/// text `doc` holds; out-of-range lines (stale markers vs a doc that shrank)
/// are skipped rather than panicking.
pub fn marker_decorations(doc: &editor_core::rope::Rope, markers: &[LineMarker]) -> Set {
    let total_bytes = doc.len_bytes();
    let total_lines = doc.len_lines();
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::with_capacity(markers.len());
    for m in markers {
        let line0 = m.line as usize;
        if line0 >= total_lines {
            continue;
        }
        let line_start = doc.line_to_byte(line0);
        let line_end = if line0 + 1 < total_lines {
            doc.line_to_byte(line0 + 1)
        } else {
            total_bytes
        };
        let range = if line_start == line_end {
            line_start..line_start + 1
        } else {
            line_start..line_end
        };
        let marker = match m.kind {
            DirtyKind::Added => GutterMarker::DiffAdded,
            DirtyKind::Modified => GutterMarker::DiffModified,
            DirtyKind::Deleted => GutterMarker::DiffRemoved,
        };
        entries.push((
            range,
            Decoration::Line(LineStyle {
                gutter_marker: Some(marker),
                ..LineStyle::default()
            }),
        ));
    }
    RangeSet::from_iter(entries)
}

impl crate::state::AppState {
    /// Refresh the dirty-diff gutter's HEAD snapshot for the buffer at `path`,
    /// off the paint path (`git-dirty-diff-gutter`, G4). Re-fetches `git show
    /// HEAD:<path>` only when the buffer's `loaded_hash` has changed since the
    /// last fetch (open / save) — so a HEAD that advanced via an integrated
    /// auto-commit-on-save is picked up on the post-save frame, while idle
    /// frames do no git work. No-op (and clears any stale snapshot) when git is
    /// disabled / the engine is absent, or the buffer isn't a plain vault file.
    ///
    /// Gating:
    /// - `git_sync` engine present ⇒ `[git].enabled` (the engine is built only
    ///   when git is on). Absent ⇒ leave `git_head_text = None` (gutter dark).
    /// - Only `BufferSource::Vault` buffers (a snapshot / trash / pending /
    ///   history preview has no meaningful "vs HEAD" comparison).
    /// - `show_at("HEAD", path)` ⇒ `Some(text)` for a tracked file; `None`
    ///   (path absent at HEAD) is an untracked / newly added file, recorded as
    ///   an empty-string base so the whole buffer reads as added. A lookup
    ///   *error* leaves the snapshot `None` (gutter dark) rather than guessing.
    pub fn refresh_dirty_diff_head(&mut self, path: &str) {
        let is_vault = self
            .session
            .buffers
            .get(path)
            .map(|b| matches!(&b.source, crate::tab::BufferSource::Vault { .. }))
            .unwrap_or(false);
        let Some(git) = self.vault_session.services.git_sync.clone() else {
            // Git off / no engine: make sure no stale snapshot keeps a gutter lit.
            if let Some(b) = self.session.buffers.get_mut(path) {
                b.git_head_text = None;
                b.git_head_refreshed_for = None;
            }
            return;
        };
        if !is_vault {
            if let Some(b) = self.session.buffers.get_mut(path) {
                b.git_head_text = None;
                b.git_head_refreshed_for = None;
            }
            return;
        }
        // Coarse refresh: only on a new loaded_hash (open / save). Read the
        // current hash, bail if we already fetched HEAD for it.
        let Some(buffer) = self.session.buffers.get(path) else { return };
        let loaded_hash = buffer.loaded_hash.clone();
        if buffer.git_head_refreshed_for.as_deref() == Some(loaded_hash.as_str()) {
            return;
        }
        // `Ok(None)` (untracked / new) ⇒ empty base (whole file added).
        // `Err(_)` ⇒ leave the snapshot absent (no gutter rather than a wrong one).
        let head = match git.show_at("HEAD", path) {
            Ok(opt) => Some(opt.unwrap_or_default()),
            Err(_) => None,
        };
        if let Some(b) = self.session.buffers.get_mut(path) {
            b.git_head_text = head;
            b.git_head_refreshed_for = Some(loaded_hash);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(head: &str, working: &str) -> Vec<(u32, DirtyKind)> {
        line_markers(head, working)
            .into_iter()
            .map(|m| (m.line, m.kind))
            .collect()
    }

    #[test]
    fn identical_is_empty() {
        assert!(line_markers("a\nb\nc\n", "a\nb\nc\n").is_empty());
        assert!(line_markers("", "").is_empty());
    }

    #[test]
    fn pure_addition_at_end() {
        // Append a line: the new last line is Added.
        let got = kinds("a\nb\n", "a\nb\nc\n");
        assert_eq!(got, vec![(2, DirtyKind::Added)]);
    }

    #[test]
    fn pure_addition_in_middle() {
        let got = kinds("a\nc\n", "a\nb\nc\n");
        assert_eq!(got, vec![(1, DirtyKind::Added)]);
    }

    #[test]
    fn modification_in_place() {
        // One line changed in place => Modified, not Add+Delete.
        let got = kinds("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(got, vec![(1, DirtyKind::Modified)]);
    }

    #[test]
    fn deletion_in_middle_anchors_on_following_line() {
        // Delete "b": the deletion caret anchors on the surviving line that
        // now sits where b was (working line 1 == "c").
        let got = kinds("a\nb\nc\n", "a\nc\n");
        assert_eq!(got, vec![(1, DirtyKind::Deleted)]);
    }

    #[test]
    fn deletion_at_eof_anchors_on_last_line() {
        // Delete the trailing line: no following surviving line, so it anchors
        // on the last surviving working line (line 1 == "b").
        let got = kinds("a\nb\nc\n", "a\nb\n");
        assert_eq!(got, vec![(1, DirtyKind::Deleted)]);
    }

    #[test]
    fn add_wins_over_colliding_delete() {
        // Replace a block such that an addition and a deletion target the same
        // after-line; the Add/Modify marker must win (louder change).
        let got = line_markers("x\na\nb\n", "x\nNEW\n");
        // Line 1 changed; whatever the diff calls it, it must be a change
        // marker (Added/Modified), never silently a bare Deleted.
        assert!(got.iter().any(|m| m.line == 1
            && matches!(m.kind, DirtyKind::Added | DirtyKind::Modified)));
        assert!(!got.iter().any(|m| m.line == 1 && m.kind == DirtyKind::Deleted));
    }

    #[test]
    fn whole_file_added_when_head_empty() {
        // Untracked / new file (no HEAD content): every line reads as Added.
        let got = kinds("", "one\ntwo\n");
        assert_eq!(
            got,
            vec![(0, DirtyKind::Added), (1, DirtyKind::Added)]
        );
    }

    #[test]
    fn multiple_distinct_changes() {
        let head = "keep1\nold\nkeep2\nkeep3\n";
        let working = "keep1\nnew\nkeep2\nadded\nkeep3\n";
        let got = kinds(head, working);
        // line 1: old->new modified; line 3: "added" inserted.
        assert!(got.contains(&(1, DirtyKind::Modified)));
        assert!(got.contains(&(3, DirtyKind::Added)));
    }

    #[test]
    fn markers_are_sorted_and_unique_per_line() {
        let head = "a\nb\nc\nd\ne\n";
        let working = "a\nB\nc\nX\nY\ne\n";
        let markers = line_markers(head, working);
        let mut lines: Vec<u32> = markers.iter().map(|m| m.line).collect();
        let sorted = {
            let mut s = lines.clone();
            s.sort_unstable();
            s
        };
        assert_eq!(lines, sorted, "markers must come out sorted by line");
        lines.dedup();
        assert_eq!(lines.len(), markers.len(), "at most one marker per line");
    }
}
