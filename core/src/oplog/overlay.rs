//! Text-edit helpers shared by the pending-queue layer: interpret a
//! [`PendingOp`] as localized text spans and fold a session's queue into a
//! base text by plain string splicing. Pending edits are *text*
//! (anchored find-replace, whole-doc content, or rename) recovered from the
//! op's `metadata`; the `accepted`/`working` text only changes when an op is
//! actually accepted (see `accept_pending`).
//
// status: op-log-pending-queue
// status: op-log-two-doc-model

use super::doc;
use super::shapes::{OpKind, PendingOp};

/// Interpret one pending op as localized edit spans against `base_text` — the
/// `(byte_start, removed_len, inserted)` shape [`doc::apply_replaces`] /
/// [`apply_spans_str`] consume. Returns `None` for a rename (no text effect),
/// a drifted anchored replace (its `old_str` no longer resolves), or an op
/// carrying no recoverable edit text.
pub(super) fn op_spans(base_text: &str, op: &PendingOp) -> Option<Vec<(usize, usize, String)>> {
    if matches!(op.op_kind, OpKind::Rename { .. }) {
        return None;
    }
    // Anchored replace: resolve `old_str` to a single range, splice `new_str`.
    if let Some(old_str) = op.metadata.get("old_str").and_then(|v| v.as_str()) {
        let new_str = op
            .metadata
            .get("new_str")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        return match doc::resolve_anchor(base_text, old_str) {
            Ok(start) => Some(vec![(start, old_str.len(), new_str.to_string())]),
            // Anchor gone — the op has drifted, no spans.
            Err(_) => None,
        };
    }
    // Whole-doc content rewrite (`stage_pending_content`).
    if let Some(new_text) = op.metadata.get("new_content").and_then(|v| v.as_str()) {
        return Some(crate::merge::multi_span_delta(base_text, new_text));
    }
    // Whole-doc rewrite (the `old_str: null` branch of `stage_pending`).
    if let Some(new_str) = op.metadata.get("new_str").and_then(|v| v.as_str()) {
        return Some(crate::merge::multi_span_delta(base_text, new_str));
    }
    None
}

/// Apply `(byte_start, removed_len, inserted)` spans to `base` high-offset-first
/// (the [`doc::apply_replaces`] discipline) by plain string splicing. Spans are
/// expected ascending and non-overlapping in `base` coordinates; a span whose
/// bounds aren't valid char boundaries or run past the end is skipped
/// defensively so a drifted span can't panic.
pub(super) fn apply_spans_str(base: &str, spans: &[(usize, usize, String)]) -> String {
    let mut s = base.to_string();
    for (start, removed_len, inserted) in spans.iter().rev() {
        let start = *start;
        let end = start + *removed_len;
        if end > s.len() || !s.is_char_boundary(start) || !s.is_char_boundary(end) {
            continue;
        }
        s.replace_range(start..end, inserted);
    }
    s
}

/// Fold the session's pending ops into `base_text` in queue order, by text
/// splicing. For each op whose `session_id` matches `session` (or all ops when
/// `session` is `None`) and that `skip(pos)` does not exclude, compute its
/// [`op_spans`] against the running text and apply them; a `None` (drifted or
/// rename op) is skipped. `skip` lets `materialize_review` additionally drop
/// `op_drifted` ops; pass `|_| false` where no extra skipping is needed.
pub(super) fn fold_session_text(
    base_text: &str,
    pending: &[PendingOp],
    session: Option<&str>,
    skip: impl Fn(usize) -> bool,
) -> String {
    let mut text = base_text.to_string();
    for (pos, op) in pending.iter().enumerate() {
        let in_session = session.is_none() || op.session_id.as_deref() == session;
        if !in_session || skip(pos) {
            continue;
        }
        if let Some(spans) = op_spans(&text, op) {
            text = apply_spans_str(&text, &spans);
        }
    }
    text
}

#[cfg(test)]
mod round_trip_tests {
    use proptest::prelude::*;

    /// Strings over a small alphabet so two independently-generated values
    /// share substructure — exercising real, non-trivial diffs and the
    /// char-boundary skip path, not just full replacements of unrelated text.
    /// The alphabet spans 1-, 2-, 3- and 4-byte UTF-8 (`a`, `é`, `→`, `😀`) to
    /// stress char-boundary handling.
    fn smallish() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop::sample::select(vec!['a', 'b', 'c', ' ', '\n', 'é', '→', '😀']),
            0..40,
        )
        .prop_map(|cs| cs.into_iter().collect())
    }

    proptest! {
        /// The fold-in invariant every user/external/remote edit relies on:
        /// the minimal span delta `base`→`target` spliced back through the
        /// PRODUCTION [`super::apply_spans_str`] (defensive char-boundary skip
        /// and all) must reproduce `target` byte-for-byte. A counterexample
        /// here is the op-log-diverges-from-disk bug at its source — and is now
        /// also caught at runtime by `commit_text_edit`'s `FoldRoundTrip` guard.
        #[test]
        fn span_delta_round_trips(base in smallish(), target in smallish()) {
            let spans = crate::merge::multi_span_delta(&base, &target);
            prop_assert_eq!(super::apply_spans_str(&base, &spans), target);
        }
    }
}
