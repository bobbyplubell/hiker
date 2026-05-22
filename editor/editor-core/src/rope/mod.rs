//! Rope: persistent UTF-8 text storage on top of SumTree.
//!
//! - Byte offsets are the canonical position.
//! - char, line, and utf16 conversions are O(log n) via tree summaries.
//! - Splits at byte offsets must be on a char boundary (panics otherwise).
//!
//! Slice/replace use the underlying SumTree's persistent split + concat:
//! slice = split, split; replace = split, split, build mid, concat x 2.
//! Chunks straddling a split boundary are sliced into fresh SmolStrs so
//! the seam falls between Chunks. After a concat we merge the two seam
//! chunks if their combined size is still under CHUNK_MAX and at least one
//! side is below CHUNK_MIN — strictly local, no whole-tree rewrite.

use smol_str::SmolStr;

use crate::sumtree::{Item, Summary, SumTree};

const CHUNK_MAX: usize = 512;
const CHUNK_MIN: usize = CHUNK_MAX / 2;

#[derive(Clone, Debug)]
pub(crate) struct Chunk(pub(crate) SmolStr);

#[derive(Default, Clone, Debug)]
pub(crate) struct ChunkSummary {
    pub bytes: u32,
    pub chars: u32,
    pub lines: u32,
    pub utf16: u32,
}

impl Summary for ChunkSummary {
    fn add(&mut self, other: &Self) {
        self.bytes += other.bytes;
        self.chars += other.chars;
        self.lines += other.lines;
        self.utf16 += other.utf16;
    }
}

impl Item for Chunk {
    type Summary = ChunkSummary;
    fn summarize(&self) -> ChunkSummary {
        let s = self.0.as_str();
        let bytes = s.len() as u32;
        let mut chars = 0u32;
        let mut utf16 = 0u32;
        for c in s.chars() {
            chars += 1;
            utf16 += c.len_utf16() as u32;
        }
        let lines = s.bytes().filter(|&b| b == b'\n').count() as u32;
        ChunkSummary { bytes, chars, lines, utf16 }
    }
}

#[derive(Clone, Default)]
pub struct Rope {
    tree: SumTree<Chunk>,
}

impl Rope {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        if s.is_empty() {
            return Self::new();
        }
        let mut chunks: Vec<Chunk> = Vec::with_capacity(s.len() / CHUNK_MIN + 1);
        let mut start = 0;
        while start < s.len() {
            let mut end = (start + CHUNK_MAX).min(s.len());
            while end > start && !s.is_char_boundary(end) {
                end -= 1;
            }
            if end == start {
                // Pathological: a single char larger than CHUNK_MAX. Take it whole.
                end = start + 1;
                while end < s.len() && !s.is_char_boundary(end) {
                    end += 1;
                }
            }
            chunks.push(Chunk(SmolStr::from(&s[start..end])));
            start = end;
        }
        Self { tree: SumTree::from_items(&chunks) }
    }

    pub fn len_bytes(&self) -> usize {
        self.tree.summary().bytes as usize
    }

    /// Stable identity fingerprint of the underlying tree root. Same value
    /// across `.clone()`; changes whenever any edit produces a new tree.
    /// Cheap (Arc pointer). Hosts can use this to memoize derived data
    /// (e.g. cached decoration sets) without re-deriving on idle frames.
    pub fn content_id(&self) -> usize {
        self.tree.root_id()
    }

    pub fn len_chars(&self) -> usize {
        self.tree.summary().chars as usize
    }

    /// Total line count. An empty rope has 1 line. A rope ending in `\n` has
    /// one trailing empty line.
    pub fn len_lines(&self) -> usize {
        self.tree.summary().lines as usize + 1
    }

    pub fn len_utf16(&self) -> usize {
        self.tree.summary().utf16 as usize
    }

    pub fn is_empty(&self) -> bool {
        self.tree.summary().bytes == 0
    }

    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        let mut s = String::with_capacity(self.len_bytes());
        for chunk in self.tree.iter() {
            s.push_str(&chunk.0);
        }
        s
    }

    pub fn byte_to_char(&self, byte: usize) -> usize {
        self.check_byte(byte);
        if byte == 0 {
            return 0;
        }
        let seek = self.tree.seek(&(byte as u32), |s| s.bytes);
        let mut chars = seek.before.chars as usize;
        if let Some(chunk) = seek.item {
            let local = byte - seek.before.bytes as usize;
            assert!(chunk.0.is_char_boundary(local), "byte offset not on char boundary");
            chars += chunk.0[..local].chars().count();
        }
        chars
    }

    pub fn char_to_byte(&self, ch: usize) -> usize {
        assert!(ch <= self.len_chars(), "char index out of range");
        if ch == 0 {
            return 0;
        }
        let seek = self.tree.seek(&(ch as u32), |s| s.chars);
        let bytes = seek.before.bytes as usize;
        if let Some(chunk) = seek.item {
            let want = ch - seek.before.chars as usize;
            for (count, (i, _)) in chunk.0.char_indices().enumerate() {
                if count == want {
                    return bytes + i;
                }
            }
            bytes + chunk.0.len()
        } else {
            bytes
        }
    }

    pub fn byte_to_line(&self, byte: usize) -> usize {
        self.check_byte(byte);
        if byte == 0 {
            return 0;
        }
        let seek = self.tree.seek(&(byte as u32), |s| s.bytes);
        let mut lines = seek.before.lines as usize;
        if let Some(chunk) = seek.item {
            let local = byte - seek.before.bytes as usize;
            lines += chunk.0.as_bytes()[..local].iter().filter(|&&b| b == b'\n').count();
        }
        lines
    }

    /// Byte offset of the start of `line`. `line == len_lines()` returns
    /// `len_bytes()` (one past end).
    pub fn line_to_byte(&self, line: usize) -> usize {
        assert!(line <= self.len_lines(), "line index out of range");
        if line == 0 {
            return 0;
        }
        let total_lines = self.len_lines();
        if line == total_lines {
            return self.len_bytes();
        }
        // Find the chunk containing the (line-1)th newline (0-indexed).
        let target = (line - 1) as u32;
        let seek = self.tree.seek(&target, |s| s.lines);
        let chunk = seek.item.expect("line < len_lines so a chunk must exist");
        let nth = (line - 1) - seek.before.lines as usize;
        let mut count = 0usize;
        for (i, &b) in chunk.0.as_bytes().iter().enumerate() {
            if b == b'\n' {
                if count == nth {
                    return seek.before.bytes as usize + i + 1;
                }
                count += 1;
            }
        }
        unreachable!("seek guarantees the nth newline is in this chunk")
    }

    pub fn byte_to_utf16(&self, byte: usize) -> usize {
        self.check_byte(byte);
        if byte == 0 {
            return 0;
        }
        let seek = self.tree.seek(&(byte as u32), |s| s.bytes);
        let mut u = seek.before.utf16 as usize;
        if let Some(chunk) = seek.item {
            let local = byte - seek.before.bytes as usize;
            assert!(chunk.0.is_char_boundary(local), "byte offset not on char boundary");
            u += chunk.0[..local].chars().map(char::len_utf16).sum::<usize>();
        }
        u
    }

    pub fn utf16_to_byte(&self, utf16: usize) -> usize {
        assert!(utf16 <= self.len_utf16(), "utf16 index out of range");
        if utf16 == 0 {
            return 0;
        }
        let seek = self.tree.seek(&(utf16 as u32), |s| s.utf16);
        let bytes = seek.before.bytes as usize;
        if let Some(chunk) = seek.item {
            let want = utf16 - seek.before.utf16 as usize;
            let mut count = 0usize;
            for (i, c) in chunk.0.char_indices() {
                if count >= want {
                    return bytes + i;
                }
                count += c.len_utf16();
            }
            bytes + chunk.0.len()
        } else {
            bytes
        }
    }

    /// Split the rope at `byte`. Returns `(left, right)`. Both pieces share
    /// structure with `self` for untouched subtrees; only the boundary chunk
    /// (if `byte` falls inside one) is reallocated.
    pub fn split_at(&self, byte: usize) -> (Rope, Rope) {
        assert!(byte <= self.len_bytes());
        if byte == 0 {
            return (Rope::new(), self.clone());
        }
        if byte == self.len_bytes() {
            return (self.clone(), Rope::new());
        }

        // Find the chunk containing `byte`.
        let seek = self.tree.seek(&(byte as u32), |s| s.bytes);
        let chunk = seek.item.expect("byte < len, chunk must exist");
        let chunk_start = seek.before.bytes as usize;
        let local = byte - chunk_start;
        assert!(chunk.0.is_char_boundary(local), "rope split not on char boundary");

        // Split the SumTree so that the entire boundary chunk lands on the
        // right. We do this by splitting at `target = chunk_start` on the
        // bytes dimension: "first item where bytes > chunk_start" is exactly
        // the boundary chunk (since prior chunks summed to chunk_start).
        let (left_tree, right_tree) = self
            .tree
            .clone()
            .split(&(chunk_start as u32), |s| s.bytes);

        // Now the boundary chunk is the leftmost chunk of `right_tree`.
        // Slice it into (left_piece, right_piece) and rejoin.
        if local == 0 {
            // Clean boundary; nothing to slice.
            return (Rope { tree: left_tree }, Rope { tree: right_tree });
        }
        // Pop the boundary chunk off `right_tree`. Use a split-by-bytes:
        // the boundary chunk is the chunk whose end is at `chunk_start +
        // chunk.0.len()`. Split right_tree at that byte position (in
        // right_tree's coordinates: chunk.0.len()).
        let chunk_len = chunk.0.len();
        // target = chunk_len: the boundary chunk's probe equals chunk_len,
        // which is NOT strictly greater, so it stays in `boundary_only`
        // (left side). The next chunk pushes us past, moving it into
        // `rest_right`.
        let (boundary_only, rest_right) = right_tree.split(&(chunk_len as u32), |s| s.bytes);
        // `boundary_only` now holds exactly the boundary chunk. Re-slice it.
        let boundary_chunk = boundary_only
            .iter()
            .next()
            .expect("boundary tree has exactly one chunk");
        let (l_piece, r_piece) = boundary_chunk.0.split_at(local);

        let mut left = Rope { tree: left_tree };
        if !l_piece.is_empty() {
            left = left.push_chunk(Chunk(SmolStr::from(l_piece)));
        }
        let mut right = Rope { tree: rest_right };
        if !r_piece.is_empty() {
            right = right.prepend_chunk(Chunk(SmolStr::from(r_piece)));
        }
        (left, right)
    }

    /// Append a single chunk on the right. Used internally at split seams.
    fn push_chunk(&self, chunk: Chunk) -> Rope {
        let one = Rope {
            tree: SumTree::from_items(&[chunk]),
        };
        self.clone().concat(one)
    }

    fn prepend_chunk(&self, chunk: Chunk) -> Rope {
        let one = Rope {
            tree: SumTree::from_items(&[chunk]),
        };
        one.concat(self.clone())
    }

    /// Concatenate two ropes. After joining, the chunks immediately on
    /// either side of the seam are merged if their combined length is still
    /// within CHUNK_MAX and at least one is below CHUNK_MIN. This is the
    /// only fixup; nothing else in the tree is rewritten.
    pub fn concat(self, other: Rope) -> Rope {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        // Look at the seam: rightmost chunk of self, leftmost chunk of other.
        let last_left = self
            .tree
            .iter()
            .last()
            .expect("non-empty rope has chunks");
        let first_right = other
            .tree
            .iter()
            .next()
            .expect("non-empty rope has chunks");
        let combined_len = last_left.0.len() + first_right.0.len();
        let needs_merge = (last_left.0.len() < CHUNK_MIN || first_right.0.len() < CHUNK_MIN)
            && combined_len <= CHUNK_MAX;
        if needs_merge {
            // Pop both ends and concat with a merged chunk in the middle.
            let last_left_str = last_left.0.clone();
            let first_right_str = first_right.0.clone();
            // Drop the trailing chunk from `self`. Splitting bytes at
            // (self_len - last_chunk_len) places all chunks whose probe
            // equals that boundary on the left, and the last (boundary)
            // chunk on the right because its probe = self_len strictly
            // exceeds the target.
            let self_len = self.len_bytes();
            let last_chunk_len = last_left_str.len();
            let (self_head, _dropped) = self.tree.clone().split(
                &((self_len - last_chunk_len) as u32),
                |s| s.bytes,
            );
            // Drop the leading chunk from `other`. target = first_chunk_len
            // keeps the boundary chunk on the left (probe == target, not
            // strictly greater), so other_tail = everything past it.
            let first_chunk_len = first_right_str.len();
            let (_dropped2, other_tail) = other
                .tree
                .clone()
                .split(&(first_chunk_len as u32), |s| s.bytes);
            let mut merged = String::with_capacity(combined_len);
            merged.push_str(&last_left_str);
            merged.push_str(&first_right_str);
            let mid_tree = SumTree::from_items(&[Chunk(SmolStr::from(merged))]);
            let left = self_head.concat(mid_tree);
            let joined = left.concat(other_tail);
            return Rope { tree: joined };
        }
        Rope {
            tree: self.tree.concat(other.tree),
        }
    }

    /// Extract a byte range as a new Rope. Range must be on char boundaries.
    pub fn slice(&self, range: std::ops::Range<usize>) -> Rope {
        let start = range.start;
        let end = range.end;
        assert!(start <= end);
        assert!(end <= self.len_bytes());
        if start == end {
            return Rope::new();
        }
        let (_, tail) = self.split_at(start);
        let (mid, _) = tail.split_at(end - start);
        mid
    }

    /// Replace `range` with `text`. Returns the resulting rope (CoW).
    pub fn replace(&self, range: std::ops::Range<usize>, text: &str) -> Rope {
        assert!(range.start <= range.end);
        assert!(range.end <= self.len_bytes());

        let (left, rest) = self.split_at(range.start);
        let (_mid_old, right) = rest.split_at(range.end - range.start);
        let mid = if text.is_empty() {
            Rope::new()
        } else {
            Rope::from_str(text)
        };
        left.concat(mid).concat(right)
    }

    pub fn insert(&self, byte: usize, text: &str) -> Rope {
        self.replace(byte..byte, text)
    }

    pub fn delete(&self, range: std::ops::Range<usize>) -> Rope {
        self.replace(range, "")
    }

    /// Concatenate chunks in order. For iteration over content.
    pub fn chunks(&self) -> impl Iterator<Item = &str> {
        self.tree.iter().map(|c| c.0.as_str())
    }

    /// Materialize a single line as a String, *without* its trailing newline.
    pub fn line_str(&self, line: usize) -> String {
        assert!(line < self.len_lines());
        let start = self.line_to_byte(line);
        let raw_end = if line + 1 < self.len_lines() {
            self.line_to_byte(line + 1)
        } else {
            self.len_bytes()
        };
        let mut s = self.slice(start..raw_end).to_string();
        if s.ends_with('\n') {
            s.pop();
        }
        s
    }

    /// Bytes of a single line, without trailing newline.
    pub fn line_len_bytes(&self, line: usize) -> usize {
        assert!(line < self.len_lines());
        let start = self.line_to_byte(line);
        let end = if line + 1 < self.len_lines() {
            self.line_to_byte(line + 1)
        } else {
            self.len_bytes()
        };
        let mut len = end - start;
        if len > 0 && self.byte_at(end - 1) == b'\n' {
            len -= 1;
        }
        len
    }

    /// Raw byte at `byte`. Panics if `byte >= len_bytes`.
    pub fn byte_at(&self, byte: usize) -> u8 {
        assert!(byte < self.len_bytes());
        let seek = self.tree.seek(&(byte as u32), |s| s.bytes);
        let chunk = seek.item.expect("byte in range so chunk must exist");
        let local = byte - seek.before.bytes as usize;
        chunk.0.as_bytes()[local]
    }

    /// Next char boundary at or after `byte`. Returns `len_bytes` if past
    /// end. Caches the containing chunk on entry and walks bytes within it,
    /// only re-seeking on chunk boundary crossings.
    pub fn next_char_boundary(&self, byte: usize) -> usize {
        let len = self.len_bytes();
        if byte >= len {
            return len;
        }
        let mut p = byte + 1;
        // Locate chunk containing `p` (or `byte` if `p == len`).
        let mut seek = self.tree.seek(&(p.min(len.saturating_sub(1)) as u32), |s| s.bytes);
        let mut chunk_start = seek.before.bytes as usize;
        let mut chunk_bytes: &[u8] = seek.item.map(|c| c.0.as_bytes()).unwrap_or(&[]);
        loop {
            if p >= len {
                return len;
            }
            if p >= chunk_start + chunk_bytes.len() {
                seek = self.tree.seek(&(p as u32), |s| s.bytes);
                chunk_start = seek.before.bytes as usize;
                chunk_bytes = seek.item.map(|c| c.0.as_bytes()).unwrap_or(&[]);
                if chunk_bytes.is_empty() {
                    return len;
                }
            }
            let b = chunk_bytes[p - chunk_start];
            if (b & 0b1100_0000) != 0b1000_0000 {
                return p;
            }
            p += 1;
        }
    }

    /// Previous char boundary strictly before `byte`. Returns 0 if at
    /// start. Caches the containing chunk on entry and walks bytes within
    /// it.
    pub fn prev_char_boundary(&self, byte: usize) -> usize {
        if byte == 0 {
            return 0;
        }
        let mut p = byte - 1;
        let mut seek = self.tree.seek(&(p as u32), |s| s.bytes);
        let mut chunk_start = seek.before.bytes as usize;
        let mut chunk_bytes: &[u8] = seek.item.map(|c| c.0.as_bytes()).unwrap_or(&[]);
        loop {
            if chunk_bytes.is_empty() {
                return 0;
            }
            let b = chunk_bytes[p - chunk_start];
            if (b & 0b1100_0000) != 0b1000_0000 {
                return p;
            }
            if p == 0 {
                return 0;
            }
            p -= 1;
            if p < chunk_start {
                seek = self.tree.seek(&(p as u32), |s| s.bytes);
                chunk_start = seek.before.bytes as usize;
                chunk_bytes = seek.item.map(|c| c.0.as_bytes()).unwrap_or(&[]);
            }
        }
    }

    fn check_byte(&self, byte: usize) {
        assert!(byte <= self.len_bytes(), "byte offset out of range");
    }
}

impl From<&str> for Rope {
    fn from(s: &str) -> Self {
        Self::from_str(s)
    }
}

impl From<String> for Rope {
    fn from(s: String) -> Self {
        Self::from_str(&s)
    }
}

impl PartialEq for Rope {
    fn eq(&self, other: &Self) -> bool {
        if self.len_bytes() != other.len_bytes() {
            return false;
        }
        // Compare byte-by-byte across chunks.
        let mut a = self.chunks().flat_map(|s| s.bytes());
        let mut b = other.chunks().flat_map(|s| s.bytes());
        loop {
            match (a.next(), b.next()) {
                (Some(x), Some(y)) if x == y => continue,
                (None, None) => return true,
                _ => return false,
            }
        }
    }
}

impl Eq for Rope {}

impl std::fmt::Debug for Rope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Rope").field(&self.to_string()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() {
        let r = Rope::new();
        assert_eq!(r.len_bytes(), 0);
        assert_eq!(r.len_chars(), 0);
        assert_eq!(r.len_lines(), 1);
        assert_eq!(r.to_string(), "");
    }

    #[test]
    fn from_short_str() {
        let r = Rope::from_str("hello world");
        assert_eq!(r.len_bytes(), 11);
        assert_eq!(r.len_chars(), 11);
        assert_eq!(r.len_lines(), 1);
        assert_eq!(r.to_string(), "hello world");
    }

    #[test]
    fn from_multiline() {
        let r = Rope::from_str("a\nbb\nccc\n");
        assert_eq!(r.len_lines(), 4);
        assert_eq!(r.byte_to_line(0), 0);
        assert_eq!(r.byte_to_line(1), 0);
        assert_eq!(r.byte_to_line(2), 1);
        assert_eq!(r.byte_to_line(5), 2);
        assert_eq!(r.byte_to_line(9), 3);
        assert_eq!(r.line_to_byte(0), 0);
        assert_eq!(r.line_to_byte(1), 2);
        assert_eq!(r.line_to_byte(2), 5);
        assert_eq!(r.line_to_byte(3), 9);
        assert_eq!(r.line_to_byte(4), 9);
    }

    #[test]
    fn unicode_lengths() {
        let r = Rope::from_str("héllo 🌍");
        // 'é' = 2 bytes, '🌍' = 4 bytes (and 2 utf16)
        assert_eq!(r.len_bytes(), "héllo 🌍".len());
        assert_eq!(r.len_chars(), 7);
        assert_eq!(r.len_utf16(), 8);
    }

    #[test]
    fn large_string_chunked() {
        let s = "abc".repeat(10_000);
        let r = Rope::from_str(&s);
        assert_eq!(r.len_bytes(), s.len());
        assert_eq!(r.to_string(), s);
    }

    #[test]
    fn insert_in_middle() {
        let r = Rope::from_str("hello world");
        let r2 = r.insert(5, ", lovely");
        assert_eq!(r2.to_string(), "hello, lovely world");
        // original unchanged
        assert_eq!(r.to_string(), "hello world");
    }

    #[test]
    fn delete_range() {
        let r = Rope::from_str("hello world").delete(5..11);
        assert_eq!(r.to_string(), "hello");
    }

    #[test]
    fn replace_range() {
        let r = Rope::from_str("hello world").replace(6..11, "rope");
        assert_eq!(r.to_string(), "hello rope");
    }

    #[test]
    fn slice_round_trip() {
        let r = Rope::from_str("the quick brown fox");
        assert_eq!(r.slice(4..9).to_string(), "quick");
        assert_eq!(r.slice(0..0).to_string(), "");
        assert_eq!(r.slice(0..19).to_string(), "the quick brown fox");
    }

    #[test]
    fn many_edits_stays_correct() {
        let mut r = Rope::from_str("");
        let mut s = String::new();
        for i in 0..200 {
            let ch = ((b'a' + (i % 26) as u8) as char).to_string();
            let pos = (i * 7) % (s.len() + 1);
            r = r.insert(pos, &ch);
            s.insert_str(pos, &ch);
        }
        assert_eq!(r.to_string(), s);
    }

    #[test]
    fn char_byte_conversions() {
        let r = Rope::from_str("a é b 🌍 c");
        for (byte_idx, _) in r.to_string().char_indices() {
            let ch = r.byte_to_char(byte_idx);
            assert_eq!(r.char_to_byte(ch), byte_idx);
        }
    }

    #[test]
    fn utf16_conversions() {
        let r = Rope::from_str("a🌍b");
        // a=1 utf16, 🌍=2 utf16, b=1 utf16
        assert_eq!(r.byte_to_utf16(0), 0);
        assert_eq!(r.byte_to_utf16(1), 1);
        assert_eq!(r.byte_to_utf16(5), 3); // after 🌍
        assert_eq!(r.byte_to_utf16(6), 4);
        assert_eq!(r.utf16_to_byte(0), 0);
        assert_eq!(r.utf16_to_byte(1), 1);
        assert_eq!(r.utf16_to_byte(3), 5);
        assert_eq!(r.utf16_to_byte(4), 6);
    }

    #[test]
    fn persistent_clone_shares() {
        let r = Rope::from_str("hello");
        let r2 = r.clone();
        assert_eq!(r, r2);
    }

    #[test]
    fn split_at_round_trip() {
        let r = Rope::from_str("the quick brown fox jumps over the lazy dog");
        for at in 0..=r.len_bytes() {
            let (l, rt) = r.split_at(at);
            assert_eq!(l.len_bytes(), at, "at={}", at);
            assert_eq!(l.to_string() + &rt.to_string(), r.to_string(), "at={}", at);
        }
    }

    #[test]
    fn slice_into_chunks() {
        // Force multiple chunks.
        let s: String = (0..3000).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let r = Rope::from_str(&s);
        for &(a, b) in &[(0, 0), (0, 1), (0, 512), (500, 1000), (1023, 1024), (0, 3000), (1500, 2500)] {
            assert_eq!(r.slice(a..b).to_string(), s[a..b], "{}..{}", a, b);
        }
    }

    #[test]
    fn content_id_changes_on_edit_and_stable_on_clone() {
        let r = Rope::from_str("hello world");
        let id = r.content_id();
        assert_eq!(r.clone().content_id(), id, "clone preserves id");
        let r2 = r.insert(5, "X");
        assert_ne!(r2.content_id(), id, "edit changes id");
    }

    #[test]
    fn random_ops_match_string() {
        // Deterministic LCG so the test is reproducible.
        let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
        let next = |r: &mut u64| -> u64 {
            *r = r.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *r
        };

        let mut rope = Rope::from_str("");
        let mut s = String::new();

        for step in 0..200 {
            let op = next(&mut rng) % 4;
            let len = s.len();
            match op {
                0 => {
                    // insert
                    let pos = if len == 0 { 0 } else { (next(&mut rng) as usize) % (len + 1) };
                    let n = ((next(&mut rng) as usize) % 8) + 1;
                    let text: String = (0..n)
                        .map(|i| (b'a' + ((step + i) % 26) as u8) as char)
                        .collect();
                    rope = rope.insert(pos, &text);
                    s.insert_str(pos, &text);
                }
                1 => {
                    // delete
                    if len == 0 {
                        continue;
                    }
                    let a = (next(&mut rng) as usize) % len;
                    let b = a + 1 + (next(&mut rng) as usize) % (len - a);
                    rope = rope.delete(a..b);
                    s.replace_range(a..b, "");
                }
                2 => {
                    // slice (read-only check)
                    if len == 0 {
                        continue;
                    }
                    let a = (next(&mut rng) as usize) % len;
                    let b = a + (next(&mut rng) as usize) % (len - a + 1);
                    assert_eq!(rope.slice(a..b).to_string(), s[a..b]);
                }
                3 => {
                    // replace
                    if len == 0 {
                        continue;
                    }
                    let a = (next(&mut rng) as usize) % len;
                    let b = a + (next(&mut rng) as usize) % (len - a + 1);
                    let n = ((next(&mut rng) as usize) % 6) + 1;
                    let text: String = (0..n)
                        .map(|i| (b'A' + ((step + i) % 26) as u8) as char)
                        .collect();
                    rope = rope.replace(a..b, &text);
                    s.replace_range(a..b, &text);
                }
                _ => unreachable!(),
            }
            assert_eq!(rope.to_string(), s, "diverged after step {}", step);
            assert_eq!(rope.len_bytes(), s.len());
        }
    }

    #[test]
    fn many_single_char_inserts_into_large_rope() {
        // Perf-shaped: shouldn't be visibly slow under --release. Starts at
        // ~100KB then performs 10_000 one-char inserts at random positions.
        let base: String = (0..100_000).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let mut rope = Rope::from_str(&base);
        let mut rng: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let next = |r: &mut u64| -> u64 {
            *r = r.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *r
        };
        for i in 0..10_000usize {
            let len = rope.len_bytes();
            let pos = (next(&mut rng) as usize) % (len + 1);
            let ch = ((b'a' + (i % 26) as u8) as char).to_string();
            rope = rope.insert(pos, &ch);
        }
        assert_eq!(rope.len_bytes(), base.len() + 10_000);
    }
}
