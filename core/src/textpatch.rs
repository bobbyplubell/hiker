//! Anchored text patching: the `old_str`/`new_str` payload shape plus the
//! pure functions that resolve and apply it against note text. A patch names
//! the text to replace by an exact substring anchor rather than a byte
//! offset, so the same edit survives unrelated content shifts. The MCP
//! `edit_note` tool resolves and applies edits through here; the in-buffer
//! patch-review surface uses the same anchor-resolution rules to decide where
//! to render decorations. Pure functions, no I/O — formerly part of the
//! retired `core::staging` module, lifted out intact when staging moved onto
//! the layered doc.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Failure resolving a patch anchor against text. The only failure mode for
/// these pure helpers: the `old_str` anchor either matched zero ranges or
/// matched several without `replace_all` set.
#[derive(Debug, Error)]
pub enum Error {
    #[error("edit anchor: {0}")]
    AnchorConflict(String),
}

/// Patch payload for `edit_note`-shaped edits: replace every (or, without
/// `replace_all`, the single unique) occurrence of `old_str` with `new_str`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditPayload {
    pub old_str: String,
    pub new_str: String,
    #[serde(default)]
    pub replace_all: bool,
}

/// Apply an anchored edit to `content`, returning the patched text. Fails
/// when `old_str` matches zero ranges (`anchor_missing`) or matches more than
/// one range without `replace_all` (`anchor_not_unique`).
pub fn apply_edit(content: &str, edit: &EditPayload) -> Result<String, Error> {
    let matches = find_all_matches(content, &edit.old_str);
    if matches.is_empty() {
        return Err(Error::AnchorConflict(
            "anchor_missing: old_str not found".to_string(),
        ));
    }
    if matches.len() > 1 && !edit.replace_all {
        return Err(Error::AnchorConflict(format!(
            "anchor_not_unique: old_str matches {} ranges; pass replace_all=true to replace all",
            matches.len(),
        )));
    }
    // Apply each replacement in order, copying spans between matches
    // verbatim. Ranges are sorted by start so overlap-free, ascending
    // splicing falls out — the find_all_matches above already advances
    // past each hit, so there's nothing to dedupe here.
    let mut sorted: Vec<(usize, usize)> = matches.to_vec();
    sorted.sort_by_key(|r| r.0);
    let mut out = String::with_capacity(content.len());
    let bytes = content.as_bytes();
    let mut cursor = 0;
    for (start, end) in sorted {
        out.push_str(std::str::from_utf8(&bytes[cursor..start]).unwrap_or(""));
        out.push_str(&edit.new_str);
        cursor = end;
    }
    out.push_str(std::str::from_utf8(&bytes[cursor..]).unwrap_or(""));
    Ok(out)
}

/// Resolve a patch-edit's `old_str` against `text` and return its unique byte
/// range. Mirrors `apply_edit`'s anchor-resolution rules — fails identically
/// when the anchor is missing or non-unique without `replace_all`. With
/// `replace_all = true` and multiple matches, returns the *first* range; UI
/// callers that only need to know whether the edit can be applied should
/// inspect the `Ok` case as "some anchor exists." Used by the in-buffer
/// patch-review surface to decide where to render decorations per frame.
pub fn locate_anchor(text: &str, edit: &EditPayload) -> Result<(usize, usize), Error> {
    let matches = find_all_matches(text, &edit.old_str);
    if matches.is_empty() {
        return Err(Error::AnchorConflict(
            "anchor_missing: old_str not found".to_string(),
        ));
    }
    if matches.len() > 1 && !edit.replace_all {
        return Err(Error::AnchorConflict(format!(
            "anchor_not_unique: old_str matches {} ranges",
            matches.len(),
        )));
    }
    Ok(matches[0])
}

/// Every non-overlapping byte range where `needle` occurs in `haystack`,
/// left-to-right. Empty needle matches nothing.
pub fn find_all_matches(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let bytes = haystack.as_bytes();
    let nb = needle.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + nb.len() <= bytes.len() {
        if &bytes[i..i + nb.len()] == nb {
            out.push((i, i + nb.len()));
            i += nb.len();
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_edit_replaces_unique_anchor() {
        let out = apply_edit(
            "hello world",
            &EditPayload {
                old_str: "world".into(),
                new_str: "there".into(),
                replace_all: false,
            },
        )
        .unwrap();
        assert_eq!(out, "hello there");
    }

    #[test]
    fn apply_edit_rejects_missing_anchor() {
        let err = apply_edit(
            "hello",
            &EditPayload {
                old_str: "nope".into(),
                new_str: "x".into(),
                replace_all: false,
            },
        );
        assert!(matches!(err, Err(Error::AnchorConflict(_))));
    }

    #[test]
    fn apply_edit_rejects_ambiguous_anchor_without_replace_all() {
        let err = apply_edit(
            "a a a",
            &EditPayload {
                old_str: "a".into(),
                new_str: "b".into(),
                replace_all: false,
            },
        );
        assert!(matches!(err, Err(Error::AnchorConflict(_))));
    }

    #[test]
    fn apply_edit_replaces_all_when_flagged() {
        let out = apply_edit(
            "a a a",
            &EditPayload {
                old_str: "a".into(),
                new_str: "b".into(),
                replace_all: true,
            },
        )
        .unwrap();
        assert_eq!(out, "b b b");
    }

    #[test]
    fn locate_anchor_returns_first_range() {
        let r = locate_anchor(
            "xx target yy",
            &EditPayload {
                old_str: "target".into(),
                new_str: String::new(),
                replace_all: false,
            },
        )
        .unwrap();
        assert_eq!(&"xx target yy"[r.0..r.1], "target");
    }

    #[test]
    fn find_all_matches_empty_needle_matches_nothing() {
        assert!(find_all_matches("abc", "").is_empty());
    }
}
