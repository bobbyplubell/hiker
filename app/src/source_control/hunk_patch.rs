//! Per-hunk patch building for the Source-Control diff view (G5).
//!
//! The diff view renders a file's working-vs-HEAD change as a list of hunks,
//! each with **Stage hunk / Unstage hunk / Discard hunk** actions. Those verbs
//! ([`GitSyncEngine::stage_hunk`](crate::git_sync::GitSyncEngine::stage_hunk)
//! et al.) apply a *single-hunk unified-diff patch* to the index / working
//! tree. This module turns `(base_text, current_text, vault_path)` into that
//! list of [`DiffHunk`]s — each one carrying both the lines to render and the
//! exact patch text to hand the engine.
//!
//! The diff itself reuses `similar` (the same line-diff engine
//! `editor_core::diff` rides on), so the hunk boundaries match what the editor
//! overlay shows. Each hunk's patch is made self-contained — a `diff --git` /
//! `---` / `+++` header plus the one `@@` hunk — so libgit2's
//! `git_diff_from_buffer` + `git_apply` accept it on its own (the backend
//! reverses it for unstage/discard).

use similar::TextDiff;

/// One renderable + applyable hunk of a file's working-vs-HEAD diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// The `@@ -a,b +c,d @@` header line (for the hunk's display heading).
    pub header: String,
    /// The hunk body lines, each prefixed `+` / `-` / ` ` (added / removed /
    /// context) — newline-stripped, for rendering.
    pub lines: Vec<String>,
    /// The self-contained one-hunk unified-diff patch text the engine verbs
    /// apply (`stage_hunk` forward; `unstage_hunk` / `discard_hunk` reversed).
    pub patch: String,
}

/// Build the per-hunk diff between `base` (HEAD content) and `current`
/// (working-tree content) for `vault_path`, with `context` lines of context on
/// each side of every change. Returns one [`DiffHunk`] per change region; an
/// identical pair yields an empty list.
///
/// `vault_path` is the forward-slash vault-relative path; it's embedded in the
/// `a/<path>` `b/<path>` patch headers so the backend's apply matches the file.
pub fn build_hunks(base: &str, current: &str, vault_path: &str, context: usize) -> Vec<DiffHunk> {
    let diff = TextDiff::from_lines(base, current);
    let file_header = format!(
        "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n",
        path = vault_path
    );
    let mut out = Vec::new();
    for hunk in diff.unified_diff().context_radius(context).iter_hunks() {
        // `hunk` Displays as `@@ ... @@\n` + body lines (each already prefixed
        // and newline-terminated by similar). Split off the header for display.
        let rendered = hunk.to_string();
        let mut lines = rendered.lines();
        let Some(header) = lines.next().map(str::to_string) else {
            continue;
        };
        let body: Vec<String> = lines.map(str::to_string).collect();
        // One self-contained patch: the file header + just this hunk.
        let patch = format!("{file_header}{rendered}");
        out.push(DiffHunk { header, lines: body, patch });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_separate_edits_yield_two_hunks() {
        // Context 0 keeps the two far-apart edits in distinct hunks; a wider
        // radius would (correctly) merge them once their contexts touch.
        let base = "a\nb\nc\nd\ne\nf\n";
        let current = "a\nB\nc\nd\nE\nf\n";
        let hunks = build_hunks(base, current, "doc.md", 0);
        assert_eq!(hunks.len(), 2, "two disjoint edits → two hunks: {hunks:?}");
        // Each hunk patch is self-contained and names the file.
        for h in &hunks {
            assert!(h.patch.starts_with("diff --git a/doc.md b/doc.md\n"), "git header: {}", h.patch);
            assert!(h.patch.contains("--- a/doc.md\n+++ b/doc.md\n"), "file header: {}", h.patch);
            assert!(h.header.starts_with("@@ "), "hunk header line: {}", h.header);
        }
        // The first hunk changes `b`→`B`; the body carries both sides.
        let first = &hunks[0];
        assert!(first.lines.iter().any(|l| l == "-b"), "removed b: {:?}", first.lines);
        assert!(first.lines.iter().any(|l| l == "+B"), "added B: {:?}", first.lines);
    }

    #[test]
    fn identical_text_has_no_hunks() {
        assert!(build_hunks("same\n", "same\n", "doc.md", 3).is_empty());
    }

    #[test]
    fn a_single_edit_is_one_hunk_with_context() {
        let base = "one\ntwo\nthree\n";
        let current = "one\nTWO\nthree\n";
        let hunks = build_hunks(base, current, "n.md", 1);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        // Context lines on both sides of the change.
        assert!(h.lines.iter().any(|l| l == " one"), "leading context: {:?}", h.lines);
        assert!(h.lines.iter().any(|l| l == " three"), "trailing context: {:?}", h.lines);
        assert!(h.lines.iter().any(|l| l == "-two"));
        assert!(h.lines.iter().any(|l| l == "+TWO"));
    }
}
