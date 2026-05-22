//! Counting-allocator regression for the editor's history-tree path.
//!
//! Drives a long stream of small transactions through `Editor::apply`
//! and asserts the process heap stays under a hard ceiling. Without the
//! `MAX_REVISIONS` compaction in `history.rs`, this test would grow
//! linearly with keystroke count and trip the ceiling.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use editor_core::change::Set;
use editor_core::state::Editor;
use editor_core::transaction::Transaction;

// ---- Counting allocator -------------------------------------------------

struct CountingAllocator;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let new_cur = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            let mut peak = PEAK.load(Ordering::Relaxed);
            while new_cur > peak {
                match PEAK.compare_exchange_weak(
                    peak,
                    new_cur,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(found) => peak = found,
                }
            }
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

fn cur() -> usize {
    CURRENT.load(Ordering::Relaxed)
}
fn pk() -> usize {
    PEAK.load(Ordering::Relaxed)
}

// ---- Test ---------------------------------------------------------------

/// 10000 single-character inserts. Without history compaction this
/// would retain 10000 Revisions (each ~200 B + Set payload),
/// O(N²) clone cost in `apply`, and peak heap proportional to the
/// product. With compaction, peak stays bounded by `MAX_REVISIONS`
/// (~2000 revisions ≈ low single-digit MB).
const PEAK_CEILING_BYTES: usize = 32 * 1024 * 1024;
const ITERATIONS: usize = 10_000;

#[test]
fn editor_apply_stream_stays_under_ceiling() {
    let mut state = Editor::new("seed\n");
    PEAK.store(cur(), Ordering::Relaxed);
    let baseline_peak = pk();
    let baseline = cur();

    for i in 0..ITERATIONS {
        // Insert one byte at the document end. Vary the byte so the
        // history coalesce window doesn't merge everything into one
        // revision (which would defeat the test).
        let ch = (b'a' + (i % 26) as u8) as char;
        let pos = state.doc.len_bytes();
        let changes = Set::of(pos, vec![(pos..pos, ch.to_string())]);
        let tx = Transaction::new(changes);
        state = state.apply(tx);
    }

    let post = cur();
    let post_peak = pk();
    let added_peak = post_peak.saturating_sub(baseline_peak);
    let leaked = post.saturating_sub(baseline);
    eprintln!(
        "editor heap_ceiling: baseline={baseline} bytes, post={post} bytes, \
         baseline_peak={baseline_peak} bytes, post_peak={post_peak} bytes, \
         added_peak={added_peak} bytes, leaked={leaked} bytes, iterations={ITERATIONS}",
    );

    assert!(
        added_peak < PEAK_CEILING_BYTES,
        "editor history peak grew by {added_peak} bytes after {ITERATIONS} edits, \
         ceiling is {PEAK_CEILING_BYTES}. Likely a missing compaction or an \
         unbounded retainer.",
    );
}
