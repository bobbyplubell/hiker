//! Multi-range selection.

use smallvec::{SmallVec, smallvec};

use crate::anchor::{Anchor, Bias};
use crate::change::Set;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SelRange {
    pub anchor: Anchor,
    pub head: Anchor,
    /// Preserved horizontal position for vertical motion. View layer concern;
    /// stored here because it must survive transactions.
    pub goal_col: Option<u32>,
}

impl SelRange {
    pub const fn point(pos: usize) -> Self {
        Self {
            anchor: Anchor::at(pos, Bias::Right),
            head: Anchor::at(pos, Bias::Right),
            goal_col: None,
        }
    }

    pub const fn new(anchor: usize, head: usize) -> Self {
        Self {
            anchor: Anchor::at(anchor, if anchor <= head { Bias::Right } else { Bias::Left }),
            head: Anchor::at(head, if head < anchor { Bias::Left } else { Bias::Right }),
            goal_col: None,
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.anchor.byte == self.head.byte
    }

    pub fn start(&self) -> usize {
        self.anchor.offset().min(self.head.offset())
    }

    pub fn end(&self) -> usize {
        self.anchor.offset().max(self.head.offset())
    }

    pub fn range(&self) -> std::ops::Range<usize> {
        self.start()..self.end()
    }

    pub fn map(self, changes: &Set) -> Self {
        Self {
            anchor: self.anchor.map(changes),
            head: self.head.map(changes),
            goal_col: self.goal_col,
        }
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Selection {
    ranges: SmallVec<[SelRange; 1]>,
    main: u32,
}

impl Default for Selection {
    fn default() -> Self {
        Self::single(0)
    }
}

impl Selection {
    pub fn single(pos: usize) -> Self {
        Self { ranges: smallvec![SelRange::point(pos)], main: 0 }
    }

    pub fn from_range(r: SelRange) -> Self {
        Self { ranges: smallvec![r], main: 0 }
    }

    pub fn from_ranges(ranges: Vec<SelRange>, main: usize) -> Self {
        assert!(!ranges.is_empty(), "Selection must have at least one range");
        let mut s = Self { ranges: ranges.into_iter().collect(), main: main as u32 };
        s.normalize();
        s
    }

    pub fn ranges(&self) -> &[SelRange] {
        &self.ranges
    }

    pub fn main(&self) -> &SelRange {
        &self.ranges[self.main as usize]
    }

    pub const fn main_index(&self) -> usize {
        self.main as usize
    }

    pub fn map(&self, changes: &Set) -> Self {
        let ranges: Vec<SelRange> = self.ranges.iter().map(|r| r.map(changes)).collect();
        let main = self.main as usize;
        let mut out = Self { ranges: ranges.into_iter().collect(), main: main as u32 };
        out.normalize();
        out
    }

    /// Replace all ranges with a single point cursor.
    pub fn replace_with_point(&self, pos: usize) -> Self {
        Self::single(pos)
    }

    /// Replace all ranges with a single range.
    pub fn replace_with_range(&self, r: SelRange) -> Self {
        Self::from_range(r)
    }

    /// Sort by start position and merge overlaps. Adjusts `main` to keep
    /// pointing at the same logical range.
    pub fn normalize(&mut self) {
        if self.ranges.len() <= 1 {
            return;
        }
        // Pair each range with whether it is the current main.
        let main_idx = self.main as usize;
        let mut tagged: Vec<(SelRange, bool)> = self
            .ranges
            .iter()
            .enumerate()
            .map(|(i, r)| (*r, i == main_idx))
            .collect();
        tagged.sort_by_key(|(r, _)| r.start());

        let mut merged: Vec<(SelRange, bool)> = Vec::with_capacity(tagged.len());
        for (r, is_main) in tagged {
            if let Some((last, last_main)) = merged.last_mut() {
                if r.start() <= last.end() {
                    // Merge: keep direction of the "main" side if any, else of `last`.
                    let new_start = last.start().min(r.start());
                    let new_end = last.end().max(r.end());
                    let take_main = *last_main || is_main;
                    let reversed = last.head.byte < last.anchor.byte;
                    let (a, h) = if reversed { (new_end, new_start) } else { (new_start, new_end) };
                    *last = SelRange::new(a, h);
                    *last_main = take_main;
                    continue;
                }
            }
            merged.push((r, is_main));
        }

        let new_main = merged
            .iter()
            .position(|(_, m)| *m)
            .unwrap_or(0) as u32;
        self.ranges = merged.into_iter().map(|(r, _)| r).collect();
        self.main = new_main;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_cursor() {
        let s = Selection::single(5);
        assert_eq!(s.ranges().len(), 1);
        assert_eq!(s.main().start(), 5);
        assert_eq!(s.main().end(), 5);
        assert!(s.main().is_empty());
    }

    #[test]
    fn range_orientation() {
        let r = SelRange::new(3, 8);
        assert_eq!(r.start(), 3);
        assert_eq!(r.end(), 8);
        let r2 = SelRange::new(8, 3);
        assert_eq!(r2.start(), 3);
        assert_eq!(r2.end(), 8);
    }

    #[test]
    fn multi_cursor_normalize_overlap() {
        let s = Selection::from_ranges(
            vec![
                SelRange::new(0, 5),
                SelRange::new(3, 8),
                SelRange::new(10, 12),
            ],
            0,
        );
        assert_eq!(s.ranges().len(), 2);
        assert_eq!(s.ranges()[0].start(), 0);
        assert_eq!(s.ranges()[0].end(), 8);
        assert_eq!(s.ranges()[1].start(), 10);
    }

    #[test]
    fn map_through_insert() {
        let s = Selection::from_ranges(
            vec![SelRange::point(2), SelRange::new(5, 9)],
            0,
        );
        let c = crate::change::Set::of(20, [(0..0, "xx".to_string())]);
        let s2 = s.map(&c);
        assert_eq!(s2.ranges()[0].start(), 4);
        assert_eq!(s2.ranges()[1].range(), 7..11);
    }
}
