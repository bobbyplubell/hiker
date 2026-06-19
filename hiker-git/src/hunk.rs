//! Unified-diff patch reversal for per-hunk unstage/discard (`git-staging-ops`).
//!
//! libgit2's `git_apply` has no reverse flag, so to "un-apply" a forward hunk
//! patch (HEAD → working) we build its reverse and apply that. The transform is
//! purely textual — swap the `---`/`+++` paths (kept in `---`-then-`+++`
//! position so libgit2 still parses the header), flip the `@@` line spans, swap
//! the `diff --git a/X b/Y` order, and flip each body line's `+`/`-` prefix.
//! Context lines, `\ No newline` markers, and `index` lines pass through.
//!
//! Kept here (not in `repo.rs`) as pure functions: no `git2` involved, directly
//! unit-testable, and it keeps `repo.rs` under the file-length budget.

use crate::{GitError, Result};

/// Reverse a one-hunk unified-diff `patch` so applying the result undoes what
/// applying `patch` would have done.
pub(crate) fn reverse_patch(patch: &str) -> Result<String> {
    // The old/new file paths from the `---`/`+++` header pair, swapped on
    // reversal but kept in their `---`-then-`+++` positions (libgit2 needs the
    // `---` line first). Captured up front, emitted in place.
    let old_path = patch.lines().find_map(|l| l.strip_prefix("--- ")).unwrap_or("a/file");
    let new_path = patch.lines().find_map(|l| l.strip_prefix("+++ ")).unwrap_or("b/file");
    let mut out = String::with_capacity(patch.len());
    for line in patch.split_inclusive('\n') {
        let (body, nl) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        if body.starts_with("--- ") {
            // Keep the `---` marker; take the (swapped) new-side path.
            out.push_str("--- ");
            out.push_str(new_path);
        } else if body.starts_with("+++ ") {
            out.push_str("+++ ");
            out.push_str(old_path);
        } else if body.starts_with("diff --git ") {
            // libgit2 cross-checks `diff --git a/X b/Y` against the `---`/`+++`
            // paths; swap the two so the reversed header stays consistent.
            out.push_str("diff --git ");
            out.push_str(new_path);
            out.push(' ');
            out.push_str(old_path);
        } else if body.starts_with("@@ ") {
            out.push_str(&reverse_hunk_header(body)?);
        } else if let Some(rest) = body.strip_prefix('+') {
            out.push('-');
            out.push_str(rest);
        } else if let Some(rest) = body.strip_prefix('-') {
            out.push('+');
            out.push_str(rest);
        } else {
            // Context line, `\ No newline at end of file`, or an `index` header
            // — unchanged on reversal.
            out.push_str(body);
        }
        out.push_str(nl);
    }
    Ok(out)
}

/// Flip a `@@ -old_start,old_len +new_start,new_len @@ [section]` header to
/// `@@ -new_start,new_len +old_start,old_len @@ [section]` (the section
/// trailer, if any, is preserved).
fn reverse_hunk_header(header: &str) -> Result<String> {
    // `@@ -A,B +C,D @@ section` — split off the section after the second `@@`.
    let rest = header.strip_prefix("@@ ").unwrap_or(header);
    let Some((spans, section)) = rest.split_once(" @@") else {
        return Err(GitError::Apply(format!("bad hunk header: {header}")));
    };
    let mut parts = spans.split_whitespace();
    let (Some(old), Some(new)) = (parts.next(), parts.next()) else {
        return Err(GitError::Apply(format!("bad hunk header spans: {header}")));
    };
    // `old` is `-A,B`, `new` is `+C,D`; on reversal the new side becomes the
    // old side and vice versa, with the `-`/`+` markers swapped.
    let old_body = old.strip_prefix('-').unwrap_or(old);
    let new_body = new.strip_prefix('+').unwrap_or(new);
    Ok(format!("@@ -{new_body} +{old_body} @@{section}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverses_header_paths_spans_and_body() {
        let fwd = "diff --git a/doc.md b/doc.md\n--- a/doc.md\n+++ b/doc.md\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        let rev = reverse_patch(fwd).unwrap();
        assert_eq!(
            rev,
            "diff --git b/doc.md a/doc.md\n--- b/doc.md\n+++ a/doc.md\n@@ -1,3 +1,3 @@\n a\n+b\n-B\n c\n",
        );
    }

    #[test]
    fn reversing_twice_is_identity() {
        let fwd = "diff --git a/x.md b/x.md\n--- a/x.md\n+++ b/x.md\n@@ -2,4 +2,5 @@\n ctx\n-old\n+new1\n+new2\n more\n";
        assert_eq!(reverse_patch(&reverse_patch(fwd).unwrap()).unwrap(), fwd);
    }

    #[test]
    fn asymmetric_spans_swap() {
        // A 2-line removal becoming 1 line: spans -10,2 +10,1 flip to -10,1 +10,2.
        let h = reverse_hunk_header("@@ -10,2 +10,1 @@ fn foo()").unwrap();
        assert_eq!(h, "@@ -10,1 +10,2 @@ fn foo()");
    }

    #[test]
    fn malformed_header_errors() {
        assert!(matches!(reverse_hunk_header("@@ garbage"), Err(GitError::Apply(_))));
    }
}
