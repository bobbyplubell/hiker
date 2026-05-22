//! Persistent B-tree with cached subtree summaries. Foundation for the rope,
//! range sets, and any other summary-indexed collection.
//!
//! Supports O(log n) persistent split and concat. The split walks the tree
//! once, breaking nodes along the search path; the concat walks down the
//! spine of the taller tree, appending matched-height subtrees and
//! rebalancing as it goes back up. Both preserve Arc sharing for untouched
//! subtrees, so existing clones are never invalidated.

use std::sync::Arc;

use smallvec::SmallVec;

pub trait Summary: Default + Clone {
    fn add(&mut self, other: &Self);
}

pub trait Item: Clone {
    type Summary: Summary;
    fn summarize(&self) -> Self::Summary;
}

const LEAF_CAP: usize = 8;
const BRANCH_CAP: usize = 16;
// Note: the split algorithm temporarily produces thin spines (down to a
// single child per level) without rebalancing siblings. We collapse
// single-child roots after the split completes; the height inflation
// inside the tree is bounded by the original height and is harmless for
// concat correctness. Empirical rope ops stay O(log n).

#[derive(Clone)]
pub struct SumTree<T: Item> {
    root: Arc<Node<T>>,
}

#[derive(Clone)]
enum Node<T: Item> {
    Leaf {
        items: SmallVec<[T; LEAF_CAP]>,
        summary: T::Summary,
    },
    Inner {
        children: SmallVec<[Arc<Node<T>>; BRANCH_CAP]>,
        summary: T::Summary,
        height: u8,
    },
}

impl<T: Item> Node<T> {
    const fn summary(&self) -> &T::Summary {
        match self {
            Node::Leaf { summary, .. } | Node::Inner { summary, .. } => summary,
        }
    }

    const fn height(&self) -> u8 {
        match self {
            Node::Leaf { .. } => 0,
            Node::Inner { height, .. } => *height,
        }
    }

    fn is_empty_leaf(&self) -> bool {
        matches!(self, Node::Leaf { items, .. } if items.is_empty())
    }

}

impl<T: Item> Default for SumTree<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Item> SumTree<T> {
    pub fn new() -> Self {
        Self {
            root: Arc::new(Node::Leaf {
                items: SmallVec::new(),
                summary: T::Summary::default(),
            }),
        }
    }

    pub fn summary(&self) -> &T::Summary {
        self.root.summary()
    }

    /// Identity of the root node. Stable across `.clone()` (Arc-shared), and
    /// always changes after a mutation that produces a new root. Useful as a
    /// cheap fingerprint for memoizing on tree identity.
    pub fn root_id(&self) -> usize {
        Arc::as_ptr(&self.root) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_empty_leaf()
            || matches!(&*self.root, Node::Inner { children, .. } if children.is_empty())
    }

    pub fn from_items(items: &[T]) -> Self {
        if items.is_empty() {
            return Self::new();
        }
        let mut nodes: Vec<Arc<Node<T>>> = items
            .chunks(LEAF_CAP)
            .map(|chunk| {
                let mut summary = T::Summary::default();
                for item in chunk {
                    summary.add(&item.summarize());
                }
                let items: SmallVec<[T; LEAF_CAP]> = chunk.iter().cloned().collect();
                Arc::new(Node::Leaf { items, summary })
            })
            .collect();

        let mut height = 1u8;
        while nodes.len() > 1 {
            nodes = nodes
                .chunks(BRANCH_CAP)
                .map(|chunk| {
                    let mut summary = T::Summary::default();
                    for child in chunk {
                        summary.add(child.summary());
                    }
                    let children: SmallVec<[Arc<Node<T>>; BRANCH_CAP]> =
                        chunk.iter().cloned().collect();
                    Arc::new(Node::Inner { children, summary, height })
                })
                .collect();
            height += 1;
        }
        Self {
            root: nodes.into_iter().next().unwrap(),
        }
    }

    pub fn iter(&self) -> Iter<'_, T> {
        Iter { stack: vec![(&self.root, 0)] }
    }

    /// Persistent O(log n) concat. Walks down the spine of the taller tree
    /// to a matched-height position, then propagates splits back up.
    pub fn concat(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let left = self.root;
        let right = other.root;
        let root = concat_nodes(left, right);
        Self {
            root: collapse_singleton_root(root),
        }
    }

    /// Persistent O(log n) split along a summary dimension. Returns
    /// `(left, right)` such that the boundary lies at the first item where
    /// `target_dim(accumulated_summary)` is strictly greater than `target`
    /// after including that item.
    ///
    /// In other words: the split splits at an item boundary, with items going
    /// to the left tree until adding the next item would push the accumulated
    /// dim past `target`.
    pub fn split<D, F>(self, target: &D, target_dim: F) -> (Self, Self)
    where
        D: PartialOrd + Clone,
        F: Fn(&T::Summary) -> D + Copy,
    {
        if self.is_empty() {
            return (Self::new(), Self::new());
        }
        let total = target_dim(self.root.summary());
        if PartialOrd::le(&total, target) {
            // The whole tree fits in the left side.
            return (self, Self::new());
        }
        let (left, right) = split_node(&self.root, target, target_dim, T::Summary::default());
        (
            Self { root: collapse_singleton_root(left) },
            Self { root: collapse_singleton_root(right) },
        )
    }

    /// Find the first item where `target_dim(accumulated_summary)` is strictly
    /// greater than `target` after including that item. Returns the item plus
    /// the summary accumulated *before* it.
    ///
    /// If no such item exists (target is past the end), `item` is `None` and
    /// `before` is the full tree summary.
    pub fn seek<D, F>(&self, target: &D, target_dim: F) -> Seek<'_, T>
    where
        D: PartialOrd,
        F: Fn(&T::Summary) -> D,
    {
        let mut node = &*self.root;
        let mut acc = T::Summary::default();
        loop {
            match node {
                Node::Inner { children, .. } => {
                    let mut descended = false;
                    for child in children {
                        let mut probe = acc.clone();
                        probe.add(child.summary());
                        if PartialOrd::gt(&target_dim(&probe), target) {
                            node = child;
                            descended = true;
                            break;
                        }
                        acc = probe;
                    }
                    if !descended {
                        return Seek { item: None, before: acc };
                    }
                }
                Node::Leaf { items, .. } => {
                    for item in items {
                        let s = item.summarize();
                        let mut probe = acc.clone();
                        probe.add(&s);
                        if PartialOrd::gt(&target_dim(&probe), target) {
                            return Seek { item: Some(item), before: acc };
                        }
                        acc = probe;
                    }
                    return Seek { item: None, before: acc };
                }
            }
        }
    }
}

pub struct Seek<'a, T: Item> {
    pub item: Option<&'a T>,
    pub before: T::Summary,
}

fn make_leaf<T: Item>(items: SmallVec<[T; LEAF_CAP]>) -> Arc<Node<T>> {
    let mut summary = T::Summary::default();
    for item in &items {
        summary.add(&item.summarize());
    }
    Arc::new(Node::Leaf { items, summary })
}

fn new_branch<T: Item>(children: SmallVec<[Arc<Node<T>>; BRANCH_CAP]>, height: u8) -> Arc<Node<T>> {
    debug_assert!(!children.is_empty());
    debug_assert!(height >= 1);
    let mut summary = T::Summary::default();
    for c in &children {
        summary.add(c.summary());
    }
    Arc::new(Node::Inner { children, summary, height })
}

/// Strip degenerate roots: an Inner node with a single child collapses to
/// that child; an empty leaf stays as an empty leaf.
fn collapse_singleton_root<T: Item>(mut root: Arc<Node<T>>) -> Arc<Node<T>> {
    loop {
        let next = match &*root {
            Node::Inner { children, .. } if children.len() == 1 => children[0].clone(),
            _ => return root,
        };
        root = next;
    }
}

/// Split a node along a summary dimension. `acc_before` is the summary of
/// items to the left of this node. Returns `(left, right)` Arc<Node>s with
/// the same height as the input — they may have under-full children
/// (including empty children); callers must collapse single-child roots and
/// trust that descendants stay invariant.
fn split_node<T: Item, D, F>(
    node: &Arc<Node<T>>,
    target: &D,
    target_dim: F,
    acc_before: T::Summary,
) -> (Arc<Node<T>>, Arc<Node<T>>)
where
    D: PartialOrd + Clone,
    F: Fn(&T::Summary) -> D + Copy,
{
    match &**node {
        Node::Leaf { items, .. } => {
            let mut acc = acc_before;
            let mut split_at = items.len();
            for (i, item) in items.iter().enumerate() {
                let mut probe = acc.clone();
                probe.add(&item.summarize());
                if PartialOrd::gt(&target_dim(&probe), target) {
                    split_at = i;
                    break;
                }
                acc = probe;
            }
            let left_items: SmallVec<[T; LEAF_CAP]> = items[..split_at].iter().cloned().collect();
            let right_items: SmallVec<[T; LEAF_CAP]> = items[split_at..].iter().cloned().collect();
            (make_leaf(left_items), make_leaf(right_items))
        }
        Node::Inner { children, height, .. } => {
            let height = *height;
            let mut acc = acc_before.clone();
            let mut child_i = children.len();
            let mut split_acc = acc.clone();
            for (i, child) in children.iter().enumerate() {
                let mut probe = acc.clone();
                probe.add(child.summary());
                if PartialOrd::gt(&target_dim(&probe), target) {
                    child_i = i;
                    split_acc = acc.clone();
                    break;
                }
                acc = probe;
            }
            if child_i == children.len() {
                // Whole node goes left (shouldn't happen if total > target,
                // but handle defensively).
                let empty: SmallVec<[Arc<Node<T>>; BRANCH_CAP]> = SmallVec::new();
                let left = Arc::clone(node);
                let right = if height == 1 {
                    make_leaf(SmallVec::new())
                } else {
                    // Create an empty inner of the right height by chaining
                    // empty leaves up. To avoid producing invalid empty
                    // inner nodes, just emit a single empty leaf and let
                    // concat reconcile heights.
                    let mut r = make_leaf::<T>(SmallVec::new());
                    for h in 1..=height {
                        let mut c: SmallVec<[Arc<Node<T>>; BRANCH_CAP]> = SmallVec::new();
                        c.push(r);
                        r = Arc::new(Node::Inner {
                            children: c,
                            summary: T::Summary::default(),
                            height: h,
                        });
                    }
                    r
                };
                let _ = empty;
                return (left, right);
            }
            let (child_left, child_right) =
                split_node(&children[child_i], target, target_dim, split_acc);

            let mut left_children: SmallVec<[Arc<Node<T>>; BRANCH_CAP]> = SmallVec::new();
            for c in &children[..child_i] {
                left_children.push(c.clone());
            }
            // Only include the split-left if it's non-empty (at a clean
            // child boundary the left half can be empty).
            if !is_node_empty(&child_left) {
                left_children.push(child_left);
            }

            let mut right_children: SmallVec<[Arc<Node<T>>; BRANCH_CAP]> = SmallVec::new();
            if !is_node_empty(&child_right) {
                right_children.push(child_right);
            }
            for c in &children[child_i + 1..] {
                right_children.push(c.clone());
            }

            // Each side may now be empty, single-child, or under-full. The
            // outer caller collapses single-child roots. Empty sides are
            // represented as an empty inner at this height; concat will
            // happily absorb them.
            let left = if left_children.is_empty() {
                make_empty_at_height(height)
            } else {
                new_branch(left_children, height)
            };
            let right = if right_children.is_empty() {
                make_empty_at_height(height)
            } else {
                new_branch(right_children, height)
            };
            (left, right)
        }
    }
}

fn is_node_empty<T: Item>(node: &Arc<Node<T>>) -> bool {
    match &**node {
        Node::Leaf { items, .. } => items.is_empty(),
        Node::Inner { children, .. } => children.is_empty(),
    }
}

fn make_empty_at_height<T: Item>(height: u8) -> Arc<Node<T>> {
    if height == 0 {
        return make_leaf(SmallVec::new());
    }
    // An "empty" inner at height h: we represent it as a leaf, and concat
    // will treat it as empty. We always collapse before returning to the
    // user, so the height mismatch is fine — concat checks emptiness first.
    make_leaf(SmallVec::new())
}

/// Persistent concat. `left` and `right` may have different heights and may
/// be empty. Returns a single root whose height is `max(h_l, h_r)` or
/// `max(h_l, h_r) + 1` if the join overflowed.
fn concat_nodes<T: Item>(left: Arc<Node<T>>, right: Arc<Node<T>>) -> Arc<Node<T>> {
    if is_node_empty(&left) {
        return right;
    }
    if is_node_empty(&right) {
        return left;
    }

    let h_l = left.height();
    let h_r = right.height();

    if h_l == h_r {
        // Same height: either merge their children/items into one node (if
        // it fits) or wrap them as two children of a new parent.
        match (&*left, &*right) {
            (
                Node::Leaf { items: l_items, .. },
                Node::Leaf { items: r_items, .. },
            ) => {
                if l_items.len() + r_items.len() <= LEAF_CAP {
                    let mut items: SmallVec<[T; LEAF_CAP]> = l_items.clone();
                    items.extend(r_items.iter().cloned());
                    make_leaf(items)
                } else {
                    let mut children: SmallVec<[Arc<Node<T>>; BRANCH_CAP]> = SmallVec::new();
                    children.push(left);
                    children.push(right);
                    new_branch(children, 1)
                }
            }
            (
                Node::Inner { children: l_ch, .. },
                Node::Inner { children: r_ch, .. },
            ) => {
                if l_ch.len() + r_ch.len() <= BRANCH_CAP {
                    let mut children: SmallVec<[Arc<Node<T>>; BRANCH_CAP]> = l_ch.clone();
                    children.extend(r_ch.iter().cloned());
                    new_branch(children, h_l)
                } else {
                    let mut children: SmallVec<[Arc<Node<T>>; BRANCH_CAP]> = SmallVec::new();
                    children.push(left);
                    children.push(right);
                    new_branch(children, h_l + 1)
                }
            }
            _ => unreachable!("nodes at the same height must be both leaf or both inner"),
        }
    } else if h_l > h_r {
        merge_into_spine(Side::Right, &left, right)
    } else {
        merge_into_spine(Side::Left, &right, left)
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum Side {
    /// Taller tree is on the left; merge the smaller tree into its right spine.
    Right,
    /// Taller tree is on the right; merge the smaller tree into its left spine.
    Left,
}

/// Merge `small` into the spine of `tall` (which must be strictly taller).
/// `side` says which spine of `tall` to descend.
fn merge_into_spine<T: Item>(
    side: Side,
    tall: &Node<T>,
    small: Arc<Node<T>>,
) -> Arc<Node<T>> {
    let tall_children = match tall {
        Node::Inner { children, .. } => children.clone(),
        Node::Leaf { .. } => unreachable!("tall must be strictly taller than small"),
    };
    let height = match tall {
        Node::Inner { height, .. } => *height,
        Node::Leaf { .. } => unreachable!(),
    };

    // Pick the spine child to recurse into.
    let (spine_idx, rest): (usize, Vec<Arc<Node<T>>>) = match side {
        Side::Right => {
            let idx = tall_children.len() - 1;
            (idx, tall_children[..idx].to_vec())
        }
        Side::Left => (0, tall_children[1..].to_vec()),
    };
    let spine_child = tall_children[spine_idx].clone();

    let merged = match side {
        Side::Right => concat_nodes(spine_child, small),
        Side::Left => concat_nodes(small, spine_child),
    };
    let merged_h = merged.height();

    // Assemble new children for this level.
    let mut new_children: SmallVec<[Arc<Node<T>>; BRANCH_CAP]> = SmallVec::new();
    let extend_with_rest = |dst: &mut SmallVec<[Arc<Node<T>>; BRANCH_CAP]>| {
        for c in &rest {
            dst.push(c.clone());
        }
    };

    if merged_h == height - 1 {
        match side {
            Side::Right => {
                extend_with_rest(&mut new_children);
                new_children.push(merged);
            }
            Side::Left => {
                new_children.push(merged);
                extend_with_rest(&mut new_children);
            }
        }
        new_branch(new_children, height)
    } else if merged_h == height {
        let m_ch = match &*merged {
            Node::Inner { children, .. } => children.clone(),
            _ => unreachable!(),
        };
        // Combine rest + m_ch in the correct order for the side.
        let mut combined: Vec<Arc<Node<T>>> = Vec::new();
        match side {
            Side::Right => {
                for c in &rest {
                    combined.push(c.clone());
                }
                for c in &m_ch {
                    combined.push(c.clone());
                }
            }
            Side::Left => {
                for c in &m_ch {
                    combined.push(c.clone());
                }
                for c in &rest {
                    combined.push(c.clone());
                }
            }
        }
        if combined.len() <= BRANCH_CAP {
            for c in combined {
                new_children.push(c);
            }
            new_branch(new_children, height)
        } else {
            let mid = combined.len() / 2;
            let mut left_c: SmallVec<[Arc<Node<T>>; BRANCH_CAP]> = SmallVec::new();
            let mut right_c: SmallVec<[Arc<Node<T>>; BRANCH_CAP]> = SmallVec::new();
            for (i, c) in combined.into_iter().enumerate() {
                if i < mid {
                    left_c.push(c);
                } else {
                    right_c.push(c);
                }
            }
            let l = new_branch(left_c, height);
            let r = new_branch(right_c, height);
            let mut parent: SmallVec<[Arc<Node<T>>; BRANCH_CAP]> = SmallVec::new();
            parent.push(l);
            parent.push(r);
            new_branch(parent, height + 1)
        }
    } else {
        unreachable!(
            "concat_nodes returned unexpected height {} (parent h={})",
            merged_h, height
        );
    }
}

pub struct Iter<'a, T: Item> {
    stack: Vec<(&'a Node<T>, usize)>,
}

impl<'a, T: Item> Iterator for Iter<'a, T> {
    type Item = &'a T;
    fn next(&mut self) -> Option<&'a T> {
        loop {
            let (node, idx) = self.stack.last_mut()?;
            match node {
                Node::Leaf { items, .. } => {
                    if *idx < items.len() {
                        let item = &items[*idx];
                        *idx += 1;
                        return Some(item);
                    }
                    self.stack.pop();
                }
                Node::Inner { children, .. } => {
                    if *idx < children.len() {
                        let child: &Arc<Node<T>> = &children[*idx];
                        *idx += 1;
                        self.stack.push((child, 0));
                    } else {
                        self.stack.pop();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default, Clone, Debug, PartialEq)]
    struct CountSummary {
        count: u32,
        sum: u64,
    }

    impl Summary for CountSummary {
        fn add(&mut self, other: &Self) {
            self.count += other.count;
            self.sum += other.sum;
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    struct Num(u32);

    impl Item for Num {
        type Summary = CountSummary;
        fn summarize(&self) -> CountSummary {
            CountSummary { count: 1, sum: self.0 as u64 }
        }
    }

    #[test]
    fn empty_tree() {
        let t: SumTree<Num> = SumTree::new();
        assert!(t.is_empty());
        assert_eq!(t.summary().count, 0);
    }

    #[test]
    fn build_and_summarize() {
        let items: Vec<Num> = (0..100).map(Num).collect();
        let t = SumTree::from_items(&items);
        assert_eq!(t.summary().count, 100);
        assert_eq!(t.summary().sum, (0..100u64).sum());
    }

    #[test]
    fn iter_round_trip() {
        let items: Vec<Num> = (0..255).map(Num).collect();
        let t = SumTree::from_items(&items);
        let collected: Vec<Num> = t.iter().cloned().collect();
        assert_eq!(collected, items);
    }

    #[test]
    fn seek_by_count() {
        let items: Vec<Num> = (0..50).map(Num).collect();
        let t = SumTree::from_items(&items);
        let s = t.seek(&10u32, |s| s.count);
        assert_eq!(s.item, Some(&Num(10)));
        assert_eq!(s.before.count, 10);
    }

    #[test]
    fn seek_past_end() {
        let items: Vec<Num> = (0..5).map(Num).collect();
        let t = SumTree::from_items(&items);
        let s = t.seek(&100u32, |s| s.count);
        assert!(s.item.is_none());
        assert_eq!(s.before.count, 5);
    }

    #[test]
    fn concat() {
        let a = SumTree::from_items(&(0..30).map(Num).collect::<Vec<_>>());
        let b = SumTree::from_items(&(30..60).map(Num).collect::<Vec<_>>());
        let c = a.concat(b);
        assert_eq!(c.summary().count, 60);
        let v: Vec<u32> = c.iter().map(|n| n.0).collect();
        assert_eq!(v, (0..60).collect::<Vec<_>>());
    }

    #[test]
    fn persistent_clone_is_cheap() {
        let t = SumTree::from_items(&(0..1000).map(Num).collect::<Vec<_>>());
        let t2 = t.clone();
        assert!(Arc::ptr_eq(&t.root, &t2.root));
    }

    #[test]
    fn split_at_boundary() {
        let items: Vec<Num> = (0..100).map(Num).collect();
        let t = SumTree::from_items(&items);
        let (l, r) = t.split(&40u32, |s| s.count);
        let lv: Vec<u32> = l.iter().map(|n| n.0).collect();
        let rv: Vec<u32> = r.iter().map(|n| n.0).collect();
        assert_eq!(lv, (0..40).collect::<Vec<_>>());
        assert_eq!(rv, (40..100).collect::<Vec<_>>());
        assert_eq!(l.summary().count, 40);
        assert_eq!(r.summary().count, 60);
    }

    #[test]
    fn split_concat_round_trip() {
        let items: Vec<Num> = (0..500).map(Num).collect();
        let t = SumTree::from_items(&items);
        for &at in &[0u32, 1, 7, 8, 9, 15, 16, 17, 100, 250, 400, 499, 500] {
            let (l, r) = t.clone().split(&at, |s| s.count);
            assert_eq!(l.summary().count, at.min(500));
            let merged = l.concat(r);
            let v: Vec<u32> = merged.iter().map(|n| n.0).collect();
            assert_eq!(v, items.iter().map(|n| n.0).collect::<Vec<_>>(), "at={}", at);
        }
    }

    #[test]
    fn split_empty_left() {
        let t = SumTree::from_items(&(0..20).map(Num).collect::<Vec<_>>());
        let (l, r) = t.split(&0u32, |s| s.count);
        assert_eq!(l.summary().count, 0);
        assert_eq!(r.summary().count, 20);
    }

    #[test]
    fn concat_unequal_heights() {
        let small = SumTree::from_items(&(0..3).map(Num).collect::<Vec<_>>());
        let big = SumTree::from_items(&(3..1000).map(Num).collect::<Vec<_>>());
        let c = small.concat(big);
        let v: Vec<u32> = c.iter().map(|n| n.0).collect();
        assert_eq!(v, (0..1000).collect::<Vec<_>>());

        let big2 = SumTree::from_items(&(0..1000).map(Num).collect::<Vec<_>>());
        let small2 = SumTree::from_items(&(1000..1005).map(Num).collect::<Vec<_>>());
        let c2 = big2.concat(small2);
        let v2: Vec<u32> = c2.iter().map(|n| n.0).collect();
        assert_eq!(v2, (0..1005).collect::<Vec<_>>());
    }

    #[test]
    fn split_preserves_sharing_of_untouched_subtrees() {
        // After a split, the original tree's clone must still be valid and
        // equal to before.
        let items: Vec<Num> = (0..1000).map(Num).collect();
        let t = SumTree::from_items(&items);
        let t2 = t.clone();
        let (_l, _r) = t.split(&500u32, |s| s.count);
        // t2 unchanged
        let v: Vec<u32> = t2.iter().map(|n| n.0).collect();
        assert_eq!(v, items.iter().map(|n| n.0).collect::<Vec<_>>());
    }
}
