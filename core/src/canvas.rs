//! Canvas rename-rewrite: when a referenced note moves, rewrite the
//! `file` path of every JSON Canvas File node that pointed at it.
//!
//! A `.canvas` file is a first-class layered-doc document (`canvas-doc-kind`) but,
//! unlike boards, it has no derived index table (`board-cards-derived-table`)
//! to enumerate referrers from — that projection is deferred
//! (`canvas-search-index`). So this sweep walks the vault for `.canvas` files,
//! parses each with [`hiker_canvas::model::Canvas::from_json`], runs the pure
//! [`Canvas::rewrite_file_refs`] helper, and persists any change through the
//! SAME layered-doc user-save path the boards rename branch uses — so the rewrite
//! is a versioned, mergeable edit in the same transaction.
//!
//! Best-effort, mirroring the boards / trails posture: a `.canvas` that fails
//! to parse is skipped, a single write failure is logged and the rest of the
//! sweep proceeds, and errors never propagate to abort the wider rename pass.
//
// status: canvas-file-ref-rewrite

use hiker_canvas::model::{Canvas, NodeKind};

pub mod export;

use crate::errors::HikerError;
use crate::indexer::{IndexJob, IndexJobTx};
use crate::editing::LayeredDoc;
use crate::vault::Vault;
use crate::watcher::Watcher;

/// Rewrite File-node `file` paths across every `.canvas` document in the
/// vault after a note move. The public entry the shared rename pass
/// ([`crate::links_rename::on_note_moved`]) calls alongside the boards sweep.
/// Errors are logged inside, never propagated. Returns the count of `.canvas`
/// files whose JSON was rewritten.
///
/// status: canvas-file-ref-rewrite
pub async fn on_note_moved(
    watcher: Option<&Watcher>,
    jobs: Option<&IndexJobTx>,
    log: Option<&LayeredDoc>,
    vault: &Vault,
    from: &str,
    to: &str,
) -> usize {
    if from == to {
        return 0;
    }
    let canvases = match walk_canvas_files(vault) {
        Ok(paths) => paths,
        Err(e) => {
            tracing::warn!(error = %e, "canvas on_note_moved: walk failed");
            return 0;
        }
    };

    let mut touched = 0usize;
    for canvas_rel in canvases {
        let ctx = RewriteCtx { log, jobs, watcher, vault };
        match ctx.rewrite_file_ref(&canvas_rel, from, to).await {
            Ok(true) => touched += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, path = %canvas_rel,
                "canvas on_note_moved: file-ref rewrite failed"),
        }
    }
    touched
}

/// Vault-relative paths of every `.canvas` document that has at least one File
/// node pointing at `note`. A best-effort on-demand scan — there's no canvas
/// reference index yet (`canvas-search-index` is deferred), so this walks the
/// `.canvas` files, parses each, and keeps those referencing the note.
/// Unreadable / unparseable canvases are skipped rather than failing the whole
/// scan, the same tolerant posture as the rename sweep. status: canvas-appears-in
pub fn canvases_referencing(vault: &Vault, note: &str) -> Result<Vec<String>, HikerError> {
    let mut out = Vec::new();
    for rel in walk_canvas_files(vault)? {
        let Ok(text) = vault.read_file(&rel) else {
            continue;
        };
        let Ok(canvas) = Canvas::from_json(&text) else {
            continue;
        };
        let refers = canvas
            .nodes
            .iter()
            .any(|n| matches!(&n.kind, NodeKind::File { file, .. } if file == note));
        if refers {
            out.push(rel);
        }
    }
    Ok(out)
}

/// Walk the vault for `.canvas` files, returning their vault-relative paths.
/// Mirrors [`Vault::walk_indexable_files`]'s walker (no symlink follow,
/// pruning watcher-ignored subtrees) but matches the `.canvas` extension —
/// `.canvas` is not in `INDEXABLE_EXTENSIONS`, so the indexable walk can't be
/// reused directly.
fn walk_canvas_files(vault: &Vault) -> Result<Vec<String>, HikerError> {
    let root = vault.root();
    let mut out = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let Ok(rel) = e.path().strip_prefix(root) else {
                return true;
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if rel_str.is_empty() {
                return true; // root entry
            }
            !crate::watcher::is_ignored(&rel_str)
        });
    for entry in walker {
        let entry = entry.map_err(|e| HikerError::Io(e.to_string()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .map_err(|e| HikerError::Io(format!("strip_prefix: {e}")))?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.ends_with(".canvas") {
            out.push(rel_str);
        }
    }
    Ok(out)
}

/// Borrow-bundle for the per-canvas rewrite, mirroring the boards
/// `RewriteCtx`. Bundling the optional handles keeps `rewrite_file_ref` a
/// method under the argument-count budget.
struct RewriteCtx<'a> {
    log: Option<&'a LayeredDoc>,
    jobs: Option<&'a IndexJobTx>,
    watcher: Option<&'a Watcher>,
    vault: &'a Vault,
}

impl RewriteCtx<'_> {
    /// Read + parse the `.canvas` doc, rewrite every File node whose `file ==
    /// from` to `to`, and persist. Returns `true` when a rewrite landed.
    /// A `.canvas` that fails to parse is skipped (returns `Ok(false)`) so a
    /// single malformed canvas never aborts the sweep. Persistence goes
    /// through the layered-doc user-save path when a log is attached, matching the
    /// boards branch; without a log it falls back to a suppressed
    /// `write_file` for CLI / test paths.
    async fn rewrite_file_ref(
        &self,
        canvas_rel: &str,
        from: &str,
        to: &str,
    ) -> Result<bool, HikerError> {
        let src = self.vault.read_file(canvas_rel)?;
        let Ok(mut canvas) = Canvas::from_json(&src) else {
            tracing::debug!(path = %canvas_rel,
                "canvas on_note_moved: skipping unparseable .canvas");
            return Ok(false);
        };
        if !canvas.rewrite_file_refs(from, to) {
            return Ok(false);
        }
        let new_src = canvas.to_canonical_json();
        match self.log {
            Some(log) => {
                crate::ops::op_writes::user_save(log, self.vault, canvas_rel, &new_src)?;
            }
            None => {
                if let Some(w) = self.watcher {
                    w.suppress(canvas_rel.to_string());
                }
                self.vault.write_file(canvas_rel, &new_src)?;
                if let Some(w) = self.watcher {
                    w.suppress(canvas_rel.to_string());
                }
            }
        }
        if let Some(j) = self.jobs {
            let _ = j
                .send(IndexJob::Upsert {
                    rel_path: canvas_rel.to_string(),
                    force: false,
                })
                .await;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
