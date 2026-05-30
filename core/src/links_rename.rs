//! Shared rename-rewrite pass over every cross-document referrer to a moved
//! note — wikilink bodies, trail waypoint paths, and kanban card paths all
//! flow through one entry point so the indexer doesn't have to know about
//! the three referrer types separately.
//!
//! Per `wikilink-rename-rewrite`, renaming a note rewrites every
//! `[[…]]` body, every `hiker.references.path` in waypoint-notes (and
//! `hiker.in_trail.path` / `hiker.waypoints[].path` mirrors), and every
//! `cards[].path` in board-docs. The indexer's referrer enumeration is the
//! load-bearing query: trails read from `store::trails_containing_note`,
//! boards read from `store::boards_containing_note`, wikilinks currently
//! re-derive by scanning the vault (the spec'd reverse-edge index for
//! `wikilink-backlinks` lands as part of this slice's follow-up; the
//! straightforward enumerator is built first per
//! `wikilink-rename-bloom-filter-deferred`).
//!
//! Each rewrite is a small frontmatter or body edit on its own document and
//! commits through the user-save path with watcher suppression. The whole
//! batch is best-effort: a single rewrite failure is logged and the rest of
//! the batch proceeds, mirroring the per-domain helpers' existing posture.
//!
//! status: wikilink-rename-rewrite

use std::sync::Arc;

use tokio::sync::OnceCell;

use crate::indexer::IndexJobTx;
use crate::oplog::OpLog;
use crate::store::Store;
use crate::vault::Vault;
use crate::watcher::Watcher;
use crate::wikilink::{self, AmbiguityPolicy, ParsedLink, Resolution};

/// Run every referrer-rewrite pass after a successful path remap. Calls the
/// trail-side helper, the board-side helper, and the wikilink-body
/// rewriter in turn; each one is independently best-effort (errors logged,
/// never propagated). Returns the total count of touched files across all
/// three types — handy for tests and trace logs.
///
/// `watcher_cell` and `oplog_cell` mirror the indexer's lazy-init scheme:
/// CLI / unit tests run without one or both attached and each helper
/// degrades to a write-without-suppress / read-without-oplog shape.
///
/// status: wikilink-rename-rewrite
pub async fn on_note_moved(
    watcher_cell: &Arc<OnceCell<Arc<Watcher>>>,
    oplog_cell: &Arc<OnceCell<Arc<OpLog>>>,
    jobs: &IndexJobTx,
    vault: &Vault,
    store: &mut Store,
    from: &str,
    to: &str,
) -> usize {
    if from == to {
        return 0;
    }
    let watcher_arc = watcher_cell.get().cloned();
    let watcher_ref = watcher_arc.as_deref();
    let log_arc = oplog_cell.get().cloned();
    let log_ref = log_arc.as_deref();

    let mut touched = 0usize;

    // status: trail-auto-update-on-note-move
    // Trails: rewrite waypoint-notes' `hiker.references.path`, plus the
    // trail-doc/waypoint mirror cases when the moved doc is itself a trail
    // or a waypoint. Errors swallowed inside the helper.
    match crate::trails::ops::on_note_moved(
        watcher_ref, Some(jobs), log_ref, vault, store, from, to,
    )
    .await
    {
        Ok(n) => touched += n,
        Err(e) => tracing::warn!(error = %e, %from, %to,
            "shared rename-rewrite: trails sweep failed"),
    }

    // status: board-card-references
    // Boards: rewrite `cards[].path` entries pointing at the moved note.
    match crate::boards::on_note_moved(
        watcher_ref, Some(jobs), log_ref, vault, store, from, to,
    )
    .await
    {
        Ok(n) => touched += n,
        Err(e) => tracing::warn!(error = %e, %from, %to,
            "shared rename-rewrite: boards sweep failed"),
    }

    // status: wikilink-rename-rewrite
    // Wikilinks: rewrite every `[[…]]` body whose resolved path was `from`
    // to the new shortest-unambiguous form for `to`.
    touched += rewrite_wikilink_bodies(watcher_ref, jobs, vault, from, to).await;

    touched
}

/// Walk every indexable `.md` note in the vault, parse its `[[…]]` links,
/// and rewrite any link whose resolved path equals `from` to point at `to`.
/// The replacement form is `wikilink::shortest_unambiguous(paths, to)` — the
/// same picker rule the autocomplete uses on insert. Each rewritten file
/// goes through the watcher-suppress + write + reindex path that the trail
/// / board helpers already use.
///
/// Bloom-filter optimization (`wikilink-rename-bloom-filter-deferred`) is
/// intentionally skipped — the straightforward enumerator lands first; a
/// per-note "contains any `[[`" filter is a follow-up if profiling shows
/// the writes-per-rename are hot. The path-set used for resolution is the
/// post-rename world (callers invoke this after `vault::move_note` /
/// `move_folder` returns Ok, so `to` is on disk and `from` is not).
async fn rewrite_wikilink_bodies(
    watcher: Option<&Watcher>,
    jobs: &IndexJobTx,
    vault: &Vault,
    from: &str,
    to: &str,
) -> usize {
    let Ok(paths) = vault.walk_indexable_files("") else {
        return 0;
    };
    let new_form = wikilink::shortest_unambiguous(&paths, to);
    let mut touched = 0usize;
    for rel in &paths {
        // A note's own body never references the note itself meaningfully
        // for this purpose; skip to avoid rewriting a stale `[[from]]` that
        // the user intentionally left.
        if rel == to {
            continue;
        }
        let Ok(body) = vault.read_file(rel) else {
            continue;
        };
        if !body.contains("[[") {
            continue;
        }
        let parsed = wikilink::parse_links(&body);
        if parsed.is_empty() {
            continue;
        }
        let policy = AmbiguityPolicy::Unresolved;
        // Collect every link span that resolved to the old path. Use a
        // reverse walk so byte offsets stay valid as we splice replacements.
        let rewrites: Vec<&ParsedLink> = parsed
            .iter()
            .filter(|l| matches!(
                wikilink::resolve_path(&paths, &l.target, policy, Some(rel)),
                Resolution::Resolved(p) if p == from,
            ))
            .collect();
        if rewrites.is_empty() {
            continue;
        }
        let new_body = splice_link_bodies(&body, &rewrites, &new_form);
        if new_body == body {
            continue;
        }
        if let Err(e) =
            crate::trails::ops::write_with_suppress_and_reindex_for_links(
                watcher, Some(jobs), vault, rel, &new_body,
            )
            .await
        {
            tracing::warn!(error = %e, path = %rel,
                "wikilink rename-rewrite: write failed");
            continue;
        }
        touched += 1;
    }
    touched
}

/// Replace every link span in `rewrites` with `[[<new_form>]]`. Walks in
/// reverse byte order so earlier offsets stay valid through later splices.
fn splice_link_bodies(body: &str, rewrites: &[&ParsedLink], new_form: &str) -> String {
    let mut out = body.to_string();
    let replacement = format!("[[{new_form}]]");
    let mut spans: Vec<std::ops::Range<usize>> =
        rewrites.iter().map(|l| l.span.clone()).collect();
    spans.sort_by_key(|s| std::cmp::Reverse(s.start));
    for span in spans {
        out.replace_range(span, &replacement);
    }
    out
}
