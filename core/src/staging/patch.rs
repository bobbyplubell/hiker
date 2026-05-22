//! Patch-edit anchoring: the `old_str`/`new_str` payload shape plus the
//! pure functions that resolve and apply it against note text. Shared by
//! the staging operations (accept/recheck) and surfaced to the in-buffer
//! patch-review UI, which needs the same anchor-resolution rules to decide
//! where to render decorations.

use serde::{Deserialize, Serialize};

use super::error::Error;

/// Patch payload for `edit_note`-shaped proposals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditPayload {
    pub old_str: String,
    pub new_str: String,
    #[serde(default)]
    pub replace_all: bool,
}

/// status: staging-per-edit-proposals
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
