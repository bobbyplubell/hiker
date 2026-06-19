//! Pure-text helpers for the layered-doc editing path. `accepted` and `working`
//! are plain `String`s (no CRDT): materialization is the identity over text,
//! so the on-disk `.md` equals `accepted` byte-for-byte. `tombstone` is carried
//! as a flag on `DocState` — there is no side `meta` map. This module holds the
//! small plain-text primitives the edit verbs share: `resolve_anchor` and the
//! `Materialized` pair.
//
// status: op-log-document-shape
// status: op-log-materialization
// status: op-log-path-identity

use super::error::Error;

/// Pure read of a document's editable state: `text` is the file's bytes
/// verbatim (no parse/re-emit), `tombstone` the delete flag. Drives every
/// diff render, save-to-disk, and accept dry-run. With `accepted`/`working`
/// now plain `String`s, this is just the `(text, tombstone)` pair the commit
/// and history paths hand around.
///
/// status: op-log-materialization
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Materialized {
    pub text: String,
    pub tombstone: bool,
}

// NOTE: the layered document `kind` helper (`kind_for`) lives in
// `crate::ops::op_writes` (the producer seam that uses it); the duplicate here
// was only ever exercised by its own test and went with the `.ops`-engine
// cleanup (`hiker-core-rework-plan.md` WS1).

/// Resolve `old_str` to exactly one byte range in `materialized`. Mirrors
/// the staging anchor contract: zero matches or (without `replace_all`)
/// multiple matches are an anchor conflict. Returns the first range's start.
///
/// status: op-log-pending-queue
pub(super) fn resolve_anchor(materialized: &str, old_str: &str) -> Result<usize, Error> {
    let mut matches = materialized.match_indices(old_str);
    let first = matches
        .next()
        .ok_or_else(|| Error::Anchor(format!("no match for old_str ({} bytes)", old_str.len())))?;
    if matches.next().is_some() {
        return Err(Error::Anchor(
            "old_str matched multiple times without replace_all".to_string(),
        ));
    }
    Ok(first.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_anchor_single_vs_multiple() {
        assert_eq!(resolve_anchor("hello world", "world").unwrap(), 6);
        assert!(resolve_anchor("a a", "a").is_err());
        assert!(resolve_anchor("hello", "zzz").is_err());
    }
}
