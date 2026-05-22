//! Set: list of (Retain | Insert | Delete) operations describing an edit.
//!
//! Modeled on CodeMirror 6 / Helix. ChangeSets compose, invert, and map
//! positions through edits.

use smol_str::SmolStr;

use crate::anchor::Bias;
use crate::rope::Rope;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Retain(u32),
    Delete(u32),
    Insert(SmolStr),
}

#[derive(Clone, Debug)]
pub struct Set {
    ops: Vec<Op>,
    len_before: u32,
    len_after: u32,
}

impl Set {
    pub fn empty(doc_len: usize) -> Self {
        let mut cs = Self { ops: Vec::new(), len_before: 0, len_after: 0 };
        if doc_len > 0 {
            cs.push(Op::Retain(doc_len as u32));
        }
        cs
    }

    /// Build from a sorted, non-overlapping list of (range, replacement).
    pub fn of(doc_len: usize, edits: impl IntoIterator<Item = (std::ops::Range<usize>, String)>) -> Self {
        let mut cs = Self { ops: Vec::new(), len_before: 0, len_after: 0 };
        let mut cursor = 0usize;
        for (range, text) in edits {
            assert!(range.start >= cursor, "edits must be sorted and non-overlapping");
            assert!(range.end <= doc_len, "edit out of range");
            if range.start > cursor {
                cs.push(Op::Retain((range.start - cursor) as u32));
            }
            let delete_len = range.end - range.start;
            if delete_len > 0 {
                cs.push(Op::Delete(delete_len as u32));
            }
            if !text.is_empty() {
                cs.push(Op::Insert(SmolStr::from(text)));
            }
            cursor = range.end;
        }
        if cursor < doc_len {
            cs.push(Op::Retain((doc_len - cursor) as u32));
        }
        cs
    }

    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    pub const fn len_before(&self) -> usize {
        self.len_before as usize
    }

    pub const fn len_after(&self) -> usize {
        self.len_after as usize
    }

    pub fn is_identity(&self) -> bool {
        self.ops.iter().all(|op| matches!(op, Op::Retain(_)))
    }

    fn push(&mut self, op: Op) {
        match op {
            Op::Retain(n) => {
                self.len_before += n;
                self.len_after += n;
                if let Some(Op::Retain(prev)) = self.ops.last_mut() {
                    *prev += n;
                    return;
                }
                self.ops.push(Op::Retain(n));
            }
            Op::Delete(n) => {
                self.len_before += n;
                if let Some(Op::Delete(prev)) = self.ops.last_mut() {
                    *prev += n;
                    return;
                }
                self.ops.push(Op::Delete(n));
            }
            Op::Insert(s) => {
                self.len_after += s.len() as u32;
                if let Some(Op::Insert(prev)) = self.ops.last_mut() {
                    let mut combined = String::with_capacity(prev.len() + s.len());
                    combined.push_str(prev);
                    combined.push_str(&s);
                    *prev = SmolStr::from(combined);
                    return;
                }
                self.ops.push(Op::Insert(s));
            }
        }
    }

    pub fn apply(&self, rope: &Rope) -> Rope {
        assert_eq!(
            rope.len_bytes(),
            self.len_before as usize,
            "Set length mismatch"
        );
        let mut out = String::with_capacity(self.len_after as usize);
        let mut cursor = 0usize;
        for op in &self.ops {
            match op {
                Op::Retain(n) => {
                    let end = cursor + *n as usize;
                    out.push_str(&rope.slice(cursor..end).to_string());
                    cursor = end;
                }
                Op::Delete(n) => {
                    cursor += *n as usize;
                }
                Op::Insert(s) => {
                    out.push_str(s);
                }
            }
        }
        Rope::from_str(&out)
    }

    /// Produce a Set that, applied to the *output* of self, returns the
    /// original input. Captures deleted text from `before`.
    pub fn invert(&self, before: &Rope) -> Set {
        assert_eq!(before.len_bytes(), self.len_before as usize);
        let mut inv = Set { ops: Vec::new(), len_before: 0, len_after: 0 };
        let mut cursor = 0usize;
        for op in &self.ops {
            match op {
                Op::Retain(n) => {
                    inv.push(Op::Retain(*n));
                    cursor += *n as usize;
                }
                Op::Delete(n) => {
                    let text = before.slice(cursor..cursor + *n as usize).to_string();
                    inv.push(Op::Insert(SmolStr::from(text)));
                    cursor += *n as usize;
                }
                Op::Insert(s) => {
                    inv.push(Op::Delete(s.len() as u32));
                }
            }
        }
        inv
    }

    /// Map a byte position through this change. `bias` decides which side of
    /// an insertion at exactly `pos` the position should land on.
    pub fn map_pos(&self, pos: usize, bias: Bias) -> usize {
        let mut in_pos = 0usize;
        let mut out_pos = 0usize;
        for op in &self.ops {
            match op {
                Op::Retain(n) => {
                    let n = *n as usize;
                    if pos < in_pos + n {
                        return out_pos + (pos - in_pos);
                    }
                    in_pos += n;
                    out_pos += n;
                }
                Op::Delete(n) => {
                    let n = *n as usize;
                    if pos < in_pos + n {
                        return out_pos;
                    }
                    in_pos += n;
                }
                Op::Insert(s) => {
                    if pos == in_pos {
                        return match bias {
                            Bias::Left => out_pos,
                            Bias::Right => out_pos + s.len(),
                        };
                    }
                    out_pos += s.len();
                }
            }
        }
        out_pos.min(self.len_after as usize)
    }

    /// Compose two changesets: self then other. Result has the same effect as
    /// applying self followed by other.
    pub fn compose(&self, other: &Set) -> Set {
        assert_eq!(
            self.len_after, other.len_before,
            "compose length mismatch: {} vs {}",
            self.len_after, other.len_before,
        );

        let mut out = Set { ops: Vec::new(), len_before: 0, len_after: 0 };
        let mut a = OpCursor::new(&self.ops);
        let mut b = OpCursor::new(&other.ops);

        loop {
            match (a.peek(), b.peek()) {
                (None, None) => break,
                // Deletions in `self` pass straight through; they only affect input.
                (Some(Op::Delete(n)), _) => {
                    out.push(Op::Delete(n));
                    a.advance();
                }
                // Inserts in `other` are pure additions to output.
                (_, Some(Op::Insert(s))) => {
                    out.push(Op::Insert(s.clone()));
                    b.advance();
                }
                (None, Some(Op::Delete(_))) | (None, Some(Op::Retain(_))) => {
                    panic!("compose: `other` consumes more than `self` produces");
                }
                (Some(_), None) => {
                    panic!("compose: `self` produces more than `other` consumes");
                }
                (Some(op_a), Some(op_b)) => {
                    let len_a = match &op_a {
                        Op::Retain(n) => *n as usize,
                        Op::Insert(s) => s.len(),
                        Op::Delete(_) => 0,
                    };
                    let len_b = match &op_b {
                        Op::Retain(n) | Op::Delete(n) => *n as usize,
                        Op::Insert(_) => 0,
                    };
                    let take = len_a.min(len_b);
                    match (op_a, op_b) {
                        (Op::Retain(_), Op::Retain(_)) => {
                            out.push(Op::Retain(take as u32));
                        }
                        (Op::Retain(_), Op::Delete(_)) => {
                            out.push(Op::Delete(take as u32));
                        }
                        (Op::Insert(s), Op::Retain(_)) => {
                            // Keep the first `take` bytes of the insertion.
                            // Char-boundary safety: insertions came from legal
                            // char-boundary edits.
                            let kept = &s[0..take];
                            out.push(Op::Insert(SmolStr::from(kept)));
                        }
                        (Op::Insert(_), Op::Delete(_)) => {
                            // Both cancel out; nothing emitted.
                        }
                        (Op::Delete(_), _) => unreachable!(),
                        (_, Op::Insert(_)) => unreachable!(),
                    }
                    a.consume(take);
                    b.consume(take);
                }
            }
        }
        out
    }
}

/// Cursor over a slice of Ops that allows partial consumption.
struct OpCursor<'a> {
    ops: &'a [Op],
    idx: usize,
    /// Bytes already consumed from the current op (0 means none).
    offset: usize,
}

impl<'a> OpCursor<'a> {
    const fn new(ops: &'a [Op]) -> Self {
        Self { ops, idx: 0, offset: 0 }
    }

    fn peek(&self) -> Option<Op> {
        let op = self.ops.get(self.idx)?;
        Some(match op {
            Op::Retain(n) => Op::Retain(*n - self.offset as u32),
            Op::Delete(n) => Op::Delete(*n - self.offset as u32),
            Op::Insert(s) => {
                if self.offset == 0 {
                    Op::Insert(s.clone())
                } else {
                    Op::Insert(SmolStr::from(&s[self.offset..]))
                }
            }
        })
    }

    const fn advance(&mut self) {
        self.idx += 1;
        self.offset = 0;
    }

    fn consume(&mut self, n: usize) {
        let cur_len = match &self.ops[self.idx] {
            Op::Retain(x) | Op::Delete(x) => *x as usize,
            Op::Insert(s) => s.len(),
        };
        self.offset += n;
        if self.offset >= cur_len {
            self.advance();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(doc_len: usize, edits: &[(std::ops::Range<usize>, &str)]) -> Set {
        Set::of(
            doc_len,
            edits.iter().map(|(r, s)| (r.clone(), s.to_string())),
        )
    }

    #[test]
    fn empty_identity() {
        let c = Set::empty(10);
        assert!(c.is_identity());
        let r = Rope::from_str("0123456789");
        assert_eq!(c.apply(&r), r);
    }

    #[test]
    fn simple_insert() {
        let r = Rope::from_str("hello world");
        let c = cs(11, &[(5..5, ",")]);
        assert_eq!(c.apply(&r).to_string(), "hello, world");
    }

    #[test]
    fn simple_delete() {
        let r = Rope::from_str("hello, world");
        let c = cs(12, &[(5..6, "")]);
        assert_eq!(c.apply(&r).to_string(), "hello world");
    }

    #[test]
    fn simple_replace() {
        let r = Rope::from_str("hello world");
        let c = cs(11, &[(6..11, "rope")]);
        assert_eq!(c.apply(&r).to_string(), "hello rope");
    }

    #[test]
    fn multi_edit_in_order() {
        let r = Rope::from_str("hello world");
        let c = cs(11, &[(0..0, "<"), (5..5, ","), (11..11, ">")]);
        assert_eq!(c.apply(&r).to_string(), "<hello, world>");
    }

    #[test]
    fn invert_round_trip() {
        let r = Rope::from_str("hello world");
        let c = cs(11, &[(0..5, "HELLO"), (6..11, "WORLD")]);
        let r2 = c.apply(&r);
        assert_eq!(r2.to_string(), "HELLO WORLD");
        let inv = c.invert(&r);
        let r3 = inv.apply(&r2);
        assert_eq!(r3, r);
    }

    #[test]
    fn map_pos_through_insert() {
        let c = cs(10, &[(3..3, "abc")]);
        assert_eq!(c.map_pos(0, Bias::Right), 0);
        assert_eq!(c.map_pos(3, Bias::Left), 3);
        assert_eq!(c.map_pos(3, Bias::Right), 6);
        assert_eq!(c.map_pos(5, Bias::Right), 8);
        assert_eq!(c.map_pos(10, Bias::Right), 13);
    }

    #[test]
    fn map_pos_through_delete() {
        let c = cs(10, &[(3..7, "")]);
        assert_eq!(c.map_pos(0, Bias::Right), 0);
        assert_eq!(c.map_pos(3, Bias::Right), 3);
        assert_eq!(c.map_pos(5, Bias::Right), 3); // inside deletion
        assert_eq!(c.map_pos(7, Bias::Right), 3);
        assert_eq!(c.map_pos(10, Bias::Right), 6);
    }

    #[test]
    fn compose_two_inserts() {
        let r = Rope::from_str("hello");
        let a = cs(5, &[(0..0, "<")]);
        let b = cs(6, &[(6..6, ">")]);
        let composed = a.compose(&b);
        assert_eq!(composed.apply(&r).to_string(), "<hello>");
    }

    #[test]
    fn compose_insert_then_delete_same_region() {
        let r = Rope::from_str("hello");
        let a = cs(5, &[(2..2, "XX")]);
        // r is now "heXXllo" (len 7); delete the XX (positions 2..4)
        let b = cs(7, &[(2..4, "")]);
        let composed = a.compose(&b);
        assert_eq!(composed.apply(&r).to_string(), "hello");
    }

    #[test]
    fn compose_then_apply_matches_sequential() {
        let r = Rope::from_str("the quick brown fox");
        let a = cs(19, &[(4..9, "lazy"), (10..15, "red")]);
        // r is now "the lazy red fox" (len 16)
        let after_a = a.apply(&r);
        assert_eq!(after_a.len_bytes(), 16);
        let b = cs(16, &[(0..3, "a"), (16..16, "!")]);
        let after_b = b.apply(&after_a);
        let composed = a.compose(&b);
        assert_eq!(composed.apply(&r), after_b);
    }
}
