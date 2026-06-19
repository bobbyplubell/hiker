//! Shared rename-rewrite pass over every cross-document referrer to a moved
//! note — wikilink bodies, trail waypoint paths, kanban card paths, and
//! list-like `refs` entries all flow through one entry point so the indexer
//! doesn't have to know about the referrer types separately.
//!
//! Per `wikilink-rename-rewrite`, renaming a note rewrites every
//! `[[…]]` body, every `hiker.references.path` in waypoint-notes (and
//! `hiker.in_trail.path` / `hiker.waypoints[].path` mirrors), every
//! `cards[].path` in board-docs, and every `hiker.refs[].path` in
//! list-like notes (epics / plans, `pm-epic-derived-table`). The indexer's
//! referrer enumeration is the load-bearing query: trails read from
//! `store::trails_containing_note`, boards from
//! `store::boards_containing_note`, lists from
//! `store::lists_containing_note`, wikilinks currently re-derive by
//! scanning the vault (the spec'd reverse-edge index for
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
use crate::editing::LayeredDoc;
use crate::store::Store;
use crate::vault::Vault;
use crate::watcher::Watcher;
use crate::wikilink::{self, AmbiguityPolicy, ParsedLink, Resolution};

/// Borrow-bundle of the long-lived handles the rename-rewrite pass reads.
/// The cells mirror the indexer's lazy-init scheme: CLI / unit tests run
/// without some attached and each helper degrades to a
/// write-without-suppress / read-without-layered / plain-boards-only shape.
/// `kinds_cell` lets the board sweep enumerate sprint-kind boards
/// (`sprint-board-subtype`) alongside plain boards.
pub struct RenameSweepCtx<'a> {
    pub watcher_cell: &'a Arc<OnceCell<Arc<Watcher>>>,
    pub layered_cell: &'a Arc<OnceCell<Arc<LayeredDoc>>>,
    pub kinds_cell: &'a Arc<OnceCell<Arc<crate::kinds::Registry>>>,
    pub jobs: &'a IndexJobTx,
    pub vault: &'a Vault,
}

/// Run every referrer-rewrite pass after a successful path remap. Calls the
/// trail-side helper, the board-side helper, and the wikilink-body
/// rewriter in turn; each one is independently best-effort (errors logged,
/// never propagated). Returns the total count of touched files across all
/// three types — handy for tests and trace logs.
///
/// status: wikilink-rename-rewrite
pub async fn on_note_moved(
    ctx: &RenameSweepCtx<'_>,
    store: &mut Store,
    from: &str,
    to: &str,
) -> usize {
    if from == to {
        return 0;
    }
    let vault = ctx.vault;
    let jobs = ctx.jobs;
    let watcher_arc = ctx.watcher_cell.get().cloned();
    let watcher_ref = watcher_arc.as_deref();
    let log_arc = ctx.layered_cell.get().cloned();
    let log_ref = log_arc.as_deref();
    let kinds_arc = ctx.kinds_cell.get().cloned();
    let kinds_ref = kinds_arc.as_deref();

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
    // Boards: rewrite `cards[].path` entries pointing at the moved note —
    // on plain boards and (registry attached) sprint-kind boards alike.
    let board_env = crate::boards::ops::NoteMovedEnv {
        watcher: watcher_ref,
        jobs: Some(jobs),
        log: log_ref,
        kinds: kinds_ref,
        vault,
    };
    match crate::boards::on_note_moved(&board_env, store, from, to).await {
        Ok(n) => touched += n,
        Err(e) => tracing::warn!(error = %e, %from, %to,
            "shared rename-rewrite: boards sweep failed"),
    }

    // status: pm-epic-derived-table
    // List-like notes (epic / plan): rewrite `hiker.refs[].path` entries
    // pointing at the moved note, enumerated from the derived `list_refs`
    // table — the fourth referrer type.
    let lists_env = crate::pm::ListsMovedEnv {
        watcher: watcher_ref,
        jobs: Some(jobs),
        log: log_ref,
        kinds: kinds_ref,
        vault,
    };
    touched += run_lists_sweep(&lists_env, store, from, to).await;

    // status: canvas-file-ref-rewrite
    // Canvas: rewrite every JSON Canvas File-node `file` path pointing at the
    // moved note across every `.canvas` document in the vault. No derived
    // referrer index exists (deferred `canvas-search-index`), so the sweep
    // walks the vault for `.canvas` files itself.
    touched +=
        crate::canvas::on_note_moved(watcher_ref, Some(jobs), log_ref, vault, from, to).await;

    // status: wikilink-rename-rewrite
    // Wikilinks: rewrite every `[[…]]` body whose resolved path was `from`
    // to the new shortest-unambiguous form for `to`.
    touched += rewrite_wikilink_bodies(watcher_ref, jobs, vault, from, to).await;

    touched
}

/// The list-refs arm of the sweep, with the shared best-effort posture:
/// errors are logged, never propagated, and count as zero touched files.
///
/// status: pm-epic-derived-table
async fn run_lists_sweep(
    env: &crate::pm::ListsMovedEnv<'_>,
    store: &mut Store,
    from: &str,
    to: &str,
) -> usize {
    match crate::pm::on_note_moved(env, store, from, to).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, %from, %to,
                "shared rename-rewrite: lists sweep failed");
            0
        }
    }
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
    let paths = match vault.walk_indexable_files("") {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, %from, %to,
                "wikilink rename-rewrite: vault walk failed; NO links were \
                 rewritten — references to the old path remain stale");
            return 0;
        }
    };
    let new_form = wikilink::shortest_unambiguous(&paths, to);
    let mut touched = 0usize;
    let mut failed = 0usize;
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
        let Some(new_body) = rewritten_body(&paths, rel, &body, from, &new_form) else {
            continue;
        };
        if let Err(e) =
            crate::trails::ops::write_with_suppress_and_reindex_for_links(
                watcher, Some(jobs), vault, rel, &new_body,
            )
            .await
        {
            tracing::warn!(error = %e, path = %rel,
                "wikilink rename-rewrite: write failed");
            failed += 1;
            continue;
        }
        touched += 1;
    }
    // A partial sweep is worse than the per-file warns make it look: every
    // skipped file keeps a `[[link]]` to a path that no longer exists, and
    // nothing retries. Summarize at error level so the partial state is one
    // grep away instead of N scattered warns.
    if failed > 0 {
        tracing::error!(%from, %to, touched, failed,
            "wikilink rename-rewrite finished PARTIALLY: some notes still \
             reference the old path");
    }
    touched
}

/// The body-scan step of [`rewrite_wikilink_bodies`], pure over one note's
/// `body`: parse its `[[…]]` links, keep the spans that resolve to `from`
/// (under the same `AmbiguityPolicy::Unresolved` rule the index uses), and
/// splice in `new_form`. Returns `None` when the note needs no rewrite —
/// no `[[` at all, no links resolving to `from`, or a splice that turns out
/// byte-identical.
fn rewritten_body(
    paths: &[String],
    rel: &str,
    body: &str,
    from: &str,
    new_form: &str,
) -> Option<String> {
    if !body.contains("[[") {
        return None;
    }
    let parsed = wikilink::parse_links(body);
    if parsed.is_empty() {
        return None;
    }
    let policy = AmbiguityPolicy::Unresolved;
    // Collect every link span that resolved to the old path. Use a
    // reverse walk so byte offsets stay valid as we splice replacements.
    let rewrites: Vec<&ParsedLink> = parsed
        .iter()
        .filter(|l| matches!(
            wikilink::resolve_path(paths, &l.target, policy, Some(rel)),
            Resolution::Resolved(p) if p == from,
        ))
        .collect();
    if rewrites.is_empty() {
        return None;
    }
    let new_body = splice_link_bodies(body, &rewrites, new_form);
    if new_body == body {
        return None;
    }
    Some(new_body)
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
