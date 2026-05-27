//! Persistent set of `(Range<Anchor>, T)` pairs.
//!
//! v1 stores entries in a sorted `Arc<Vec>`. The API matches what the future
//! SumTree-backed version will expose; switching the backing store is a
//! drop-in replacement.

use std::sync::Arc;

use crate::anchor::{Anchor, Bias};
use crate::change::Set;

/// Whether a value, when stored in a [`RangeSet`], can change the height of
/// the line(s) it covers. Used to decide — once, at set-construction time —
/// whether the set must be scanned by the heightmap driver. The default is
/// `false`; only [`crate::decoration::Decoration`] overrides it (for the
/// hide / height-scale / block variants).
///
/// Computing this at construction (rather than at the push call site) is what
/// removes the "wrong push method" footgun: a set knows for itself whether it
/// affects height, so the layer container can route it to a height layer
/// automatically while the heightmap driver still scans only those layers.
pub trait HeightAffecting {
    /// True if this value can change a line's height when used as a
    /// decoration. Defaults to `false` for non-decoration payloads.
    fn affects_height(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug)]
pub struct RangeSet<T: Clone> {
    entries: Arc<Vec<Entry<T>>>,
    /// Largest (`end - start`) seen across `entries`. Used by
    /// [`Self::iter_overlapping`] as a bound to narrow the scan window
    /// to entries that *could* reach a given query — turns the previous
    /// O(N) per-call linear scan into ~O(log N + K). Cached at
    /// construction; copied through `insert` / `map`.
    max_extent: u32,
    /// True if any stored value reports [`HeightAffecting::affects_height`].
    /// Computed once at construction and carried through `insert` / `map`, so
    /// the heightmap driver never has to re-derive it per frame.
    affects_height: bool,
}

#[derive(Clone, Debug)]
struct Entry<T> {
    start: Anchor,
    end: Anchor,
    value: T,
}

impl<T: Clone> Default for RangeSet<T> {
    fn default() -> Self {
        Self::empty()
    }
}

impl<T: Clone> RangeSet<T> {
    pub fn empty() -> Self {
        Self {
            entries: Arc::new(Vec::new()),
            max_extent: 0,
            affects_height: false,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Stable identity fingerprint (the entries Arc's pointer). Equal across
    /// `.clone()` of the same set, different after any structural mutation.
    /// Used by hosts to memoize derived data.
    pub fn content_id(&self) -> usize {
        Arc::as_ptr(&self.entries) as usize
    }

    /// True if any stored value affects line height. Computed once at
    /// construction (see [`HeightAffecting`]); the heightmap driver uses it
    /// to decide which layers it must scan.
    pub const fn affects_height(&self) -> bool {
        self.affects_height
    }

    /// Iterate all `(range, value)` pairs whose range overlaps `query`. Zero-
    /// width ranges (anchors / block decorations) are returned when their
    /// point falls inside `query`.
    pub fn iter_overlapping(
        &self,
        query: std::ops::Range<usize>,
    ) -> impl Iterator<Item = (std::ops::Range<usize>, &T)> {
        let q_start = query.start as u32;
        let q_end = query.end as u32;
        // Bound the scan window using `max_extent` as the worst-case
        // distance any entry's start sits before its end. Without this
        // we'd scan every entry in the set — paint walks visible-line ×
        // layer pairs through here, so the cumulative cost was visible
        // in scroll latency even on small docs.
        let lo_start = q_start.saturating_sub(self.max_extent);
        let lo = self
            .entries
            .partition_point(|e| e.start.byte < lo_start);
        let hi = self
            .entries
            .partition_point(|e| e.start.byte < q_end);
        self.entries[lo..hi]
            .iter()
            .filter(move |e| {
                if e.start.byte == e.end.byte {
                    e.start.byte >= q_start && e.start.byte < q_end
                } else {
                    e.end.byte > q_start && e.start.byte < q_end
                }
            })
            .map(|e| (e.start.byte as usize..e.end.byte as usize, &e.value))
    }

    pub fn iter_all(&self) -> impl Iterator<Item = (std::ops::Range<usize>, &T)> {
        self.entries
            .iter()
            .map(|e| (e.start.byte as usize..e.end.byte as usize, &e.value))
    }

}

impl<T: Clone + HeightAffecting> RangeSet<T> {
    #[allow(clippy::should_implement_trait)]
    pub fn from_iter<I: IntoIterator<Item = (std::ops::Range<usize>, T)>>(iter: I) -> Self {
        let mut entries: Vec<Entry<T>> = iter
            .into_iter()
            .map(|(r, v)| Entry {
                start: Anchor::at(r.start, Bias::Right),
                end: Anchor::at(r.end, Bias::Left),
                value: v,
            })
            .collect();
        entries.sort_by_key(|e| e.start.byte);
        let max_extent = entries
            .iter()
            .map(|e| e.end.byte.saturating_sub(e.start.byte))
            .max()
            .unwrap_or(0);
        let affects_height = entries.iter().any(|e| e.value.affects_height());
        Self {
            entries: Arc::new(entries),
            max_extent,
            affects_height,
        }
    }

    pub fn insert(&self, range: std::ops::Range<usize>, value: T) -> Self {
        let mut entries = (*self.entries).clone();
        let entry = Entry {
            start: Anchor::at(range.start, Bias::Right),
            end: Anchor::at(range.end, Bias::Left),
            value,
        };
        let entry_extent = entry.end.byte.saturating_sub(entry.start.byte);
        let entry_affects_height = entry.value.affects_height();
        let pos = entries.partition_point(|e| e.start.byte <= entry.start.byte);
        entries.insert(pos, entry);
        Self {
            entries: Arc::new(entries),
            max_extent: self.max_extent.max(entry_extent),
            affects_height: self.affects_height || entry_affects_height,
        }
    }

    /// Map all entries through `changes`. Entries whose range fully collapses
    /// (start == end after mapping) are dropped.
    pub fn map(&self, changes: &Set) -> Self {
        let mut out: Vec<Entry<T>> = Vec::with_capacity(self.entries.len());
        for e in self.entries.iter() {
            let start = e.start.map(changes);
            let end = e.end.map(changes);
            if start.byte < end.byte {
                out.push(Entry { start, end, value: e.value.clone() });
            }
        }
        out.sort_by_key(|e| e.start.byte);
        let max_extent = out
            .iter()
            .map(|e| e.end.byte.saturating_sub(e.start.byte))
            .max()
            .unwrap_or(0);
        let affects_height = out.iter().any(|e| e.value.affects_height());
        Self {
            entries: Arc::new(out),
            max_extent,
            affects_height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The generic constructors require `T: HeightAffecting`; the test
    // payload types are non-decoration scalars that never affect height.
    impl HeightAffecting for u32 {}
    impl HeightAffecting for &str {}

    #[test]
    fn empty() {
        let s: RangeSet<u32> = RangeSet::empty();
        assert!(s.is_empty());
    }

    #[test]
    fn insert_and_query() {
        let s = RangeSet::<&str>::empty()
            .insert(0..5, "a")
            .insert(10..15, "b")
            .insert(3..8, "c");
        let hits: Vec<_> = s.iter_overlapping(4..6).map(|(r, v)| (r, *v)).collect();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|(_, v)| *v == "a"));
        assert!(hits.iter().any(|(_, v)| *v == "c"));
    }

    #[test]
    fn map_through_insert() {
        let s = RangeSet::<u32>::from_iter([(0..3, 1), (5..10, 2)]);
        // Insert 2 chars at position 4
        let c = Set::of(15, [(4..4, "xx".to_string())]);
        let s2 = s.map(&c);
        let collected: Vec<_> = s2.iter_all().collect();
        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].0, 0..3);
        assert_eq!(collected[1].0, 7..12);
    }

    #[test]
    fn map_collapse_drops_entry() {
        let s = RangeSet::<u32>::from_iter([(2..5, 1)]);
        // Delete the entire range
        let c = Set::of(10, [(2..5, String::new())]);
        let s2 = s.map(&c);
        assert!(s2.is_empty());
    }
}
