//! Undo/redo history as a true undo tree.
//!
//! Each [`Revision`] carries a `parent` pointer and a `last_child` pointer.
//! New edits after an undo create a SIBLING branch rather than truncating the
//! redo path, so prior work is never silently destroyed. `redo` follows the
//! most recently created child (`last_child`); time-travel via [`History::earlier`]
//! / [`History::later`] can still reach orphaned branches by walking the tree
//! ordered by timestamp.
//!
//! Index 0 is reserved as a sentinel "root" revision with empty changes; real
//! revisions start at index 1. `head` is the index of the current revision.

use std::time::Duration;
use web_time::Instant;

use crate::change::Set;
use crate::rope::Rope;
use crate::selection::Selection;
use crate::transaction::{EditType, Transaction};

const COALESCE_WINDOW: Duration = Duration::from_millis(500);

/// Soft cap on retained revisions. When `record` would push past this,
/// we compact the tree down to the current head's ancestor chain and
/// drop every other branch + every ancestor older than `KEEP_RECENT`
/// steps. Without this cap, a long editing session against a large
/// buffer accumulates revisions unbounded (each carries forward +
/// inverse ChangeSets), and `Editor::apply` deep-clones the
/// entire revisions Vec per keystroke — a fast path to OOM.
///
/// 2000 ≈ ~1 MiB of revision metadata for typical short edits; a
/// session can still undo back ~`KEEP_RECENT` steps after compaction.
const MAX_REVISIONS: usize = 2000;
const KEEP_RECENT: usize = 1000;

#[derive(Clone)]
struct Revision {
    parent: Option<u32>,
    last_child: Option<u32>,
    forward: Set,
    inverse: Set,
    selection_before: Selection,
    selection_after: Selection,
    edit_type: Option<EditType>,
    timestamp: Instant,
}

impl Revision {
    fn root() -> Self {
        Self {
            parent: None,
            last_child: None,
            forward: Set::empty(0),
            inverse: Set::empty(0),
            selection_before: Selection::default(),
            selection_after: Selection::default(),
            edit_type: None,
            timestamp: Instant::now(),
        }
    }
}

#[derive(Clone)]
pub struct History {
    revisions: Vec<Revision>,
    /// Current revision index. 0 == root sentinel (nothing to undo).
    head: u32,
    /// Test seam: when `Some`, used in place of `Instant::now()` for the next
    /// recorded revision's timestamp. Consumed (set to None) after use.
    #[cfg(test)]
    now_override: Option<Instant>,
}

impl Default for History {
    fn default() -> Self {
        Self {
            revisions: vec![Revision::root()],
            head: 0,
            #[cfg(test)]
            now_override: None,
        }
    }
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    fn now(&mut self) -> Instant {
        #[cfg(test)]
        if let Some(t) = self.now_override.take() {
            return t;
        }
        Instant::now()
    }

    #[cfg(test)]
    const fn set_now(&mut self, t: Instant) {
        self.now_override = Some(t);
    }

    /// Record a transaction. `before` is the doc state prior to applying it
    /// (needed to compute the inverse changeset). `selection_before` is the
    /// pre-transaction selection.
    pub fn record(
        &mut self,
        before: &Rope,
        tx: &Transaction,
        selection_before: Selection,
        selection_after: Selection,
    ) {
        let inverse = tx.changes.invert(before);
        let now = self.now();

        if self.should_coalesce(tx, now) {
            let head = self.head as usize;
            let last = &mut self.revisions[head];
            last.forward = last.forward.compose(&tx.changes);
            // Inverse: apply new inverse first, then old inverse.
            last.inverse = inverse.compose(&last.inverse);
            last.selection_after = selection_after;
            last.timestamp = now;
            return;
        }

        let new_rev = Revision {
            parent: Some(self.head),
            last_child: None,
            forward: tx.changes.clone(),
            inverse,
            selection_before,
            selection_after,
            edit_type: tx.annotations.edit_type,
            timestamp: now,
        };
        let new_idx = self.revisions.len() as u32;
        self.revisions.push(new_rev);
        self.revisions[self.head as usize].last_child = Some(new_idx);
        self.head = new_idx;

        if self.revisions.len() > MAX_REVISIONS {
            self.compact_to_ancestor_chain();
        }
    }

    /// Drop every revision not on the current head's ancestor chain
    /// and keep only the most recent `KEEP_RECENT` of those. The root
    /// sentinel survives; the head retains its position relative to the
    /// retained chain. Called from `record` when revisions exceed
    /// `MAX_REVISIONS`.
    ///
    /// Sibling branches created by undo-then-edit are lost, as is the
    /// older tail of the linear undo history. The compaction is
    /// destructive — a user-visible "history was compacted" cue is not
    /// surfaced here; the alternative is silently growing to OOM.
    fn compact_to_ancestor_chain(&mut self) {
        // Collect ancestors of head, head-first.
        let mut chain: Vec<u32> = Vec::new();
        let mut cur = Some(self.head);
        while let Some(idx) = cur {
            if idx == 0 {
                break;
            }
            chain.push(idx);
            cur = self.revisions[idx as usize].parent;
        }
        // Keep at most KEEP_RECENT — `chain[0]` is the head (newest),
        // truncate the tail to drop the oldest.
        chain.truncate(KEEP_RECENT);
        // Rebuild revisions: index 0 = root, then chain in chronological
        // order (oldest first), so parent pointers index lower entries.
        chain.reverse();
        let mut new_revisions: Vec<Revision> = Vec::with_capacity(chain.len() + 1);
        new_revisions.push(Revision::root());
        let mut new_head: u32 = 0;
        for (i, old_idx) in chain.iter().enumerate() {
            let mut rev = self.revisions[*old_idx as usize].clone();
            let new_idx = (i + 1) as u32;
            rev.parent = Some(if i == 0 { 0 } else { new_idx - 1 });
            rev.last_child = if i + 1 < chain.len() {
                Some(new_idx + 1)
            } else {
                None
            };
            new_revisions.push(rev);
            new_head = new_idx;
        }
        // Patch root's last_child if we kept any revisions.
        if !chain.is_empty() {
            new_revisions[0].last_child = Some(1);
        }
        self.revisions = new_revisions;
        self.head = new_head;
    }

    fn should_coalesce(&self, tx: &Transaction, now: Instant) -> bool {
        if self.head == 0 {
            return false;
        }
        let last = &self.revisions[self.head as usize];
        // Coalescing composes `last.forward` with `tx.changes`, which is only
        // defined when the new change's pre-state length matches the last
        // revision's post-state length. If the doc advanced out-of-band since
        // the last record (e.g. the op-log binding pulled a new `working` state
        // into the editor between two keystrokes), those lengths diverge and
        // the compose would trip `Set::compose`'s invariant assert. In that
        // case never coalesce — start a fresh revision so undo stays correct.
        // This guard is unconditional: it also overrides `join_with_previous`,
        // since that flag cannot make a length-mismatched compose valid.
        if last.forward.len_after() != tx.changes.len_before() {
            return false;
        }
        if tx.annotations.join_with_previous {
            return true;
        }
        matches!(
            (last.edit_type, tx.annotations.edit_type),
            (Some(EditType::Input), Some(EditType::Input))
                | (Some(EditType::Delete), Some(EditType::Delete))
                if now.duration_since(last.timestamp) <= COALESCE_WINDOW
        )
    }

    /// Produce the transaction that undoes the current revision, or None if
    /// already at the root.
    pub fn undo(&mut self) -> Option<Transaction> {
        let rev = &self.revisions[self.head as usize];
        let parent = rev.parent?;
        let tx = Transaction::new(rev.inverse.clone())
            .with_selection(rev.selection_before.clone())
            .with_edit_type(EditType::Undo);
        self.head = parent;
        Some(tx)
    }

    /// Produce the transaction that re-applies the `last_child` of the current
    /// revision, or None if there is no child to redo onto.
    pub fn redo(&mut self) -> Option<Transaction> {
        let child_idx = self.revisions[self.head as usize].last_child?;
        let child = &self.revisions[child_idx as usize];
        let tx = Transaction::new(child.forward.clone())
            .with_selection(child.selection_after.clone())
            .with_edit_type(EditType::Redo);
        self.head = child_idx;
        Some(tx)
    }

    pub fn can_undo(&self) -> bool {
        self.revisions[self.head as usize].parent.is_some()
    }

    pub fn can_redo(&self) -> bool {
        self.revisions[self.head as usize].last_child.is_some()
    }

    /// Jump to the most recent revision whose timestamp is at least `dur`
    /// before `now()`. Walks up the parent chain composing inverses.
    pub fn earlier(&mut self, dur: Duration) -> Option<Transaction> {
        let now = self.now();
        let cutoff = now.checked_sub(dur)?;
        let start = self.head;
        // Walk up while the current revision's timestamp is newer than cutoff
        // (i.e. we want to undo it).
        let mut cur = start;
        let mut composed: Option<Set> = None;
        let mut final_selection = self.revisions[cur as usize].selection_before.clone();
        loop {
            let rev = &self.revisions[cur as usize];
            let Some(parent) = rev.parent else { break };
            if rev.timestamp <= cutoff {
                break;
            }
            composed = Some(match composed {
                None => rev.inverse.clone(),
                Some(prev) => prev.compose(&rev.inverse),
            });
            final_selection = rev.selection_before.clone();
            cur = parent;
        }
        if cur == start {
            return None;
        }
        self.head = cur;
        composed.map(|cs| {
            Transaction::new(cs)
                .with_selection(final_selection)
                .with_edit_type(EditType::Undo)
        })
    }

    /// Opposite of [`earlier`]: walk down `last_child` while child timestamps
    /// are within `dur` of `now()` (i.e. still "recent"). Composes forwards.
    pub fn later(&mut self, dur: Duration) -> Option<Transaction> {
        let now = self.now();
        let cutoff = now.checked_sub(dur)?;
        let start = self.head;
        let mut cur = start;
        let mut composed: Option<Set> = None;
        let mut final_selection = self.revisions[cur as usize].selection_after.clone();
        while let Some(child_idx) = self.revisions[cur as usize].last_child {
            let child = &self.revisions[child_idx as usize];
            // Stop if this child is older than the cutoff (shouldn't happen
            // normally since children are newer than parents, but be safe).
            if child.timestamp < cutoff {
                // We still want to include children that are newer than cutoff;
                // if a child is older it predates our window — stop.
                break;
            }
            composed = Some(match composed {
                None => child.forward.clone(),
                Some(prev) => prev.compose(&child.forward),
            });
            final_selection = child.selection_after.clone();
            cur = child_idx;
        }
        if cur == start {
            return None;
        }
        self.head = cur;
        composed.map(|cs| {
            Transaction::new(cs)
                .with_selection(final_selection)
                .with_edit_type(EditType::Redo)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::Set;

    fn insert_tx(doc_len: usize, at: usize, text: &str, edit: EditType) -> Transaction {
        let cs = Set::of(doc_len, vec![(at..at, text.to_string())]);
        Transaction::new(cs).with_edit_type(edit)
    }

    fn apply(rope: &mut Rope, tx: &Transaction) {
        *rope = tx.changes.apply(rope);
    }

    #[test]
    fn linear_undo_redo() {
        let mut rope = Rope::from_str("");
        let mut h = History::new();
        let sel0 = Selection::default();

        let tx1 = insert_tx(rope.len_bytes(), 0, "hello", EditType::Other);
        let before = rope.clone();
        apply(&mut rope, &tx1);
        h.record(&before, &tx1, sel0.clone(), sel0.clone());

        let tx2 = insert_tx(rope.len_bytes(), 5, " world", EditType::Other);
        let before = rope.clone();
        apply(&mut rope, &tx2);
        h.record(&before, &tx2, sel0.clone(), sel0.clone());

        assert_eq!(rope.to_string(), "hello world");
        assert!(h.can_undo());

        let u = h.undo().unwrap();
        apply(&mut rope, &u);
        assert_eq!(rope.to_string(), "hello");
        assert!(h.can_redo());

        let r = h.redo().unwrap();
        apply(&mut rope, &r);
        assert_eq!(rope.to_string(), "hello world");
    }

    #[test]
    fn branch_preserved_after_new_edit_post_undo() {
        // Edit A, undo, edit B, undo, undo -> root. Redo follows last_child = B.
        let mut rope = Rope::from_str("");
        let mut h = History::new();
        let sel0 = Selection::default();

        // Edit A
        let txa = insert_tx(rope.len_bytes(), 0, "A", EditType::Other);
        let before = rope.clone();
        apply(&mut rope, &txa);
        h.record(&before, &txa, sel0.clone(), sel0.clone());
        assert_eq!(rope.to_string(), "A");

        // Undo
        let u = h.undo().unwrap();
        apply(&mut rope, &u);
        assert_eq!(rope.to_string(), "");

        // Edit B (branches; A should still exist in tree)
        let txb = insert_tx(rope.len_bytes(), 0, "B", EditType::Other);
        let before = rope.clone();
        apply(&mut rope, &txb);
        h.record(&before, &txb, sel0.clone(), sel0.clone());
        assert_eq!(rope.to_string(), "B");

        // A's revision should still be in the arena.
        let a_present = h.revisions.iter().any(|r| {
            let s = r.forward.apply(&Rope::from_str("")).to_string();
            s == "A"
        });
        assert!(a_present, "branch A must be preserved in the tree");

        // Undo B -> root
        let u = h.undo().unwrap();
        apply(&mut rope, &u);
        assert_eq!(rope.to_string(), "");
        assert!(!h.can_undo(), "should be at root");

        // Redo should follow last_child = B path (the newer branch).
        let r = h.redo().unwrap();
        apply(&mut rope, &r);
        assert_eq!(rope.to_string(), "B");
    }

    #[test]
    fn earlier_jumps_back_in_time() {
        let mut rope = Rope::from_str("");
        let mut h = History::new();
        let sel0 = Selection::default();

        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(1000);
        let now_at_call = t1 + Duration::from_millis(100);

        // Edit at t0
        let tx1 = insert_tx(rope.len_bytes(), 0, "old", EditType::Other);
        let before = rope.clone();
        apply(&mut rope, &tx1);
        h.set_now(t0);
        h.record(&before, &tx1, sel0.clone(), sel0.clone());

        // Edit at t1 (with non-coalescing edit type Other but timestamp jump
        // still wouldn't coalesce since gap > window).
        let tx2 = insert_tx(rope.len_bytes(), 3, "new", EditType::Other);
        let before = rope.clone();
        apply(&mut rope, &tx2);
        h.set_now(t1);
        h.record(&before, &tx2, sel0.clone(), sel0.clone());
        assert_eq!(rope.to_string(), "oldnew");

        // earlier(500ms): cutoff = now - 500ms = t1 + 100 - 500 = t1 - 400ms.
        // t1's revision (timestamp t1) is newer than cutoff -> undo it.
        // t0's revision (timestamp t0, ~1s before cutoff) is older -> stop.
        h.set_now(now_at_call);
        let tx = h.earlier(Duration::from_millis(500)).expect("should undo t1");
        apply(&mut rope, &tx);
        assert_eq!(rope.to_string(), "old", "earlier should undo t1 only");
    }

    #[test]
    fn earlier_then_later_roundtrip() {
        let mut rope = Rope::from_str("");
        let mut h = History::new();
        let sel0 = Selection::default();

        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(1000);

        let tx1 = insert_tx(rope.len_bytes(), 0, "A", EditType::Other);
        let before = rope.clone();
        apply(&mut rope, &tx1);
        h.set_now(t0);
        h.record(&before, &tx1, sel0.clone(), sel0.clone());

        let tx2 = insert_tx(rope.len_bytes(), 1, "B", EditType::Other);
        let before = rope.clone();
        apply(&mut rope, &tx2);
        h.set_now(t1);
        h.record(&before, &tx2, sel0.clone(), sel0.clone());
        assert_eq!(rope.to_string(), "AB");

        h.set_now(t1 + Duration::from_millis(100));
        let back = h.earlier(Duration::from_millis(500)).unwrap();
        apply(&mut rope, &back);
        assert_eq!(rope.to_string(), "A");

        h.set_now(t1 + Duration::from_millis(100));
        let fwd = h.later(Duration::from_millis(2000)).unwrap();
        apply(&mut rope, &fwd);
        assert_eq!(rope.to_string(), "AB");
    }

    #[test]
    fn record_does_not_coalesce_across_out_of_band_doc_advance() {
        // Regression for `bug-concurrent-edit-compose-length-mismatch-panic`.
        //
        // The layered op-log binding pulls a new `working` state into the
        // editor's doc *out-of-band* (an accepted agent op) between two user
        // keystrokes — it mutates `editor.doc` directly, NOT via `Editor::apply`,
        // so `history` is not told. The next keystroke is recorded against the
        // advanced doc, whose length no longer matches what the last coalescing
        // candidate revision expects. Coalescing would then compose two Sets
        // whose lengths disagree (`last.forward.len_after != tx.changes.len_before`),
        // tripping `Set::compose`'s `compose length mismatch` assert.
        //
        // The fix: refuse to coalesce when the base diverged — start a fresh
        // revision instead. Undo stays correct; the assert (a true invariant)
        // is never violated from the coalesce path.
        let mut rope = Rope::from_str("");
        let mut h = History::new();
        let sel0 = Selection::default();

        let t0 = Instant::now();

        // Keystroke 1: insert "a" at 0. doc -> "a".
        let tx1 = insert_tx(rope.len_bytes(), 0, "a", EditType::Input);
        let before = rope.clone();
        apply(&mut rope, &tx1);
        h.set_now(t0);
        h.record(&before, &tx1, sel0.clone(), sel0.clone());
        assert_eq!(rope.to_string(), "a");

        // OUT-OF-BAND advance: the binding pulls a longer `working` into the doc
        // directly (an accepted agent op). History is NOT informed; its last
        // revision still thinks the post-state length is 1.
        rope = Rope::from_str("aWORKING");

        // Keystroke 2: insert "b" at the end of the advanced doc. Recorded
        // within the coalesce window with the same Input edit type, so the old
        // code would try to coalesce and compose mismatched-length Sets.
        let tx2 = insert_tx(rope.len_bytes(), rope.len_bytes(), "b", EditType::Input);
        let before = rope.clone();
        apply(&mut rope, &tx2);
        h.set_now(t0 + Duration::from_millis(10));
        // Before the fix this panics: "compose length mismatch: 1 vs 8".
        h.record(&before, &tx2, sel0.clone(), sel0.clone());
        assert_eq!(rope.to_string(), "aWORKINGb");

        // A fresh revision must have been created (no coalesce). Undoing it
        // reverts only keystroke 2, against the advanced doc — proving the
        // inverse was captured against the correct base.
        let u = h.undo().unwrap();
        apply(&mut rope, &u);
        assert_eq!(rope.to_string(), "aWORKING", "undo reverts only the post-advance keystroke");
    }
}
