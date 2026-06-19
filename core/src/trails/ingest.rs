//! Ingest-side re-derivation of the `trail_waypoints` table: the
//! indexer's per-file hook (`indexer/jobs.rs::process_upsert`) calls
//! [`update_trail_waypoints_if_relevant`] for every ingested `.md`, and
//! this module rebuilds the derived rows for trail-docs (the canonical
//! depth-first walk) and waypoint-notes (the single keyed row). Pulled
//! beside the trail parsers it consumes (`parse_trail_doc_for`,
//! `parse_waypoint`, `walk_waypoints_depth_first`) — the logic is
//! trail-domain, not scheduler plumbing. All errors are soft: ingest
//! never fails because a derived row could not be written.
//
// status: trail-waypoints-derived-table

use crate::editing::LayeredDoc;
use crate::store::Store;

/// Soft-error helper that re-derives `trail_waypoints` rows for a file
/// that may be a trail-doc or a waypoint-note.
///
/// Two ingest paths share this function:
///
///   - **Trail-doc ingest** is the authoritative re-derive: it walks
///     the recursive `hiker.waypoints` tree, clears every existing row
///     for `trail_id`, and re-inserts one row per waypoint with
///     correct `parent_waypoint_id` + `tree_path` filled in. This is
///     the canonical population path; tree-shape edits to the
///     trail-doc reach the table here.
///   - **Waypoint-note ingest** writes a single row keyed on the
///     waypoint's own frontmatter (`hiker.in_trail`, `hiker.references`,
///     `hiker.id`). It cannot know its own `parent_waypoint_id` /
///     `tree_path` from the waypoint-note alone — that information
///     lives in the parent trail-doc. So those columns are written as
///     `(NULL, "")` here; the next trail-doc ingest fills the
///     canonical values via the depth-first re-derive above. The
///     append-waypoint op enqueues both upserts (waypoint-note then
///     trail-doc), so the canonical fill follows immediately.
pub fn update_trail_waypoints_if_relevant(
    store: &mut Store,
    layered: Option<&LayeredDoc>,
    rel_path: &str,
    contents: &str,
) {
    // Cheap kind discriminator: only attempt the parse on `.md` files.
    if !rel_path.ends_with(".md") {
        return;
    }
    let ingest = WaypointIngest { store, layered, rel_path, contents };
    // status: note-companion-folder
    // Waypoints now live in the trail-doc's visible companion folder, so a
    // path prefix no longer distinguishes them from trail-docs. Dispatch on
    // `hiker.kind` instead: a note that parses as a waypoint
    // (`hiker.kind: waypoint`) writes a waypoint row; anything else routes
    // to the trail-doc rebuild path (which itself no-ops on a note that
    // isn't a trail-doc).
    if super::parse_waypoint(contents).is_ok() {
        ingest.upsert_waypoint_row();
    } else {
        ingest.rebuild_trail_doc_rows();
    }
}

/// Bundled refs for the two derived-table update paths. Methods stay exempt
/// from `clippy::single_call_fn` and split the work so the dispatcher above
/// stays under the cognitive-complexity cap.
struct WaypointIngest<'a> {
    store: &'a mut Store,
    layered: Option<&'a LayeredDoc>,
    rel_path: &'a str,
    contents: &'a str,
}

impl<'a> WaypointIngest<'a> {
    fn upsert_waypoint_row(self) {
        use crate::store::dto::WaypointRow;
        use super::parse_waypoint;
        let fm = match parse_waypoint(self.contents) {
            Ok(fm) => fm,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    path = %self.rel_path,
                    "indexer: waypoint parse failed (file may be mid-edit)",
                );
                return;
            }
        };
        // status: store-path-is-identity
        // Trail id is the layered doc's `doc_id` for `fm.in_trail` (the
        // waypoint-note's parent trail-doc path). The source note is
        // referenced by path only (path-as-identity).
        let source_path = fm.references.clone();
        let trail_id = self
            .layered
            .and_then(|log| log.doc_id_for_path(&fm.in_trail).unwrap_or(None))
            .unwrap_or_default();
        // Waypoint id (the `WaypointRow.waypoint_id` column) is
        // the layered doc's `doc_id` for the waypoint-note's own path under
        // path-as-identity — sourced from the same lookup the trail-doc
        // ingest uses to seed its row.
        let waypoint_id = self
            .layered
            .and_then(|log| log.doc_id_for_path(self.rel_path).unwrap_or(None))
            .unwrap_or_default();
        let row = WaypointRow {
            waypoint_path: self.rel_path.to_string(),
            waypoint_id,
            trail_id,
            source_path,
            // Tree-position columns are owned by the trail-doc ingest
            // path; written as the empty / NULL default here. The trail-
            // doc ingest that follows `append_waypoint` enqueues both, so
            // the canonical values land within the same indexer drain.
            parent_waypoint_id: None,
            tree_path: String::new(),
        };
        if let Err(e) = self.store.upsert_trail_waypoint(&row) {
            tracing::warn!(
                error = %e,
                path = %self.rel_path,
                "indexer: upsert_trail_waypoint failed",
            );
        }
    }

    /// Trail-doc ingest: clear + re-insert every row for `trail_id` so
    /// tree-shape changes (re-parent, reorder, remove) propagate to the
    /// derived table. Frontmatter is the source of truth.
    ///
    /// status: trail-waypoints-derived-table
    /// status: trail-side-trail-shape
    fn rebuild_trail_doc_rows(self) {
        use crate::store::dto::WaypointRow;
        use super::{parse_trail_doc_for, walk_waypoints_depth_first};
        let Ok(fm) = parse_trail_doc_for(self.rel_path, self.contents) else { return };
        // status: store-path-is-identity
        // The trail's id is the layered doc's `doc_id` for the trail-doc's
        // path; absent (layered not seeded yet) is a soft no-op so the
        // next ingest re-derives once the cell is populated.
        let Some(log) = self.layered else { return };
        let trail_id = match log.doc_id_for_path(self.rel_path) {
            Ok(Some(id)) => id,
            _ => return,
        };
        // Capture existing rows BEFORE the clear so we can preserve each
        // row's `source_path` (that column is owned by the per-waypoint
        // ingest path and isn't recoverable from the trail-doc alone).
        let existing_by_path: std::collections::HashMap<String, String> = self
            .store
            .waypoints_of(&trail_id)
            .unwrap_or_default()
            .into_iter()
            .map(|r| (r.waypoint_path, r.source_path))
            .collect();
        if let Err(e) = self.store.delete_trail_waypoints_by_trail(&trail_id) {
            tracing::warn!(
                error = %e,
                trail_id = %trail_id,
                "indexer: delete_trail_waypoints_by_trail failed",
            );
        }
        let store = self.store;
        walk_waypoints_depth_first(&fm.waypoints, &mut |parent_path, entry, tree_path| {
            let source_path = existing_by_path
                .get(&entry.path)
                .cloned()
                .unwrap_or_default();
            // status: store-path-is-identity
            // Waypoint id / parent waypoint id are the layered-doc doc_ids
            // for each waypoint-note path. Both default to empty when
            // the lookup misses — the rows still wire correctly via
            // `waypoint_path` / `source_path`.
            let waypoint_id = log
                .doc_id_for_path(&entry.path)
                .ok()
                .flatten()
                .unwrap_or_default();
            let parent_waypoint_id = parent_path
                .and_then(|p| log.doc_id_for_path(p).ok().flatten());
            let row = WaypointRow {
                waypoint_path: entry.path.clone(),
                waypoint_id,
                trail_id: trail_id.clone(),
                source_path,
                parent_waypoint_id,
                tree_path: tree_path.to_string(),
            };
            if let Err(e) = store.upsert_trail_waypoint(&row) {
                tracing::warn!(
                    error = %e,
                    path = %entry.path,
                    "indexer: upsert_trail_waypoint (trail-doc walk) failed",
                );
            }
        });
    }
}
