//! Deterministic memory-ceiling regression test for the indexer.
//!
//! Wires a `#[global_allocator]` that counts every `alloc`/`dealloc` byte
//! and tracks the running peak. The test builds a synthetic vault of
//! moderate size, runs the indexer's full-scan path against it, and
//! asserts that the *peak resident heap added by the indexer pass*
//! stays under a hard ceiling.
//!
//! This catches the "memory grows without bound" class of regression
//! (per-frame leaks, ever-growing caches, missing drops) without
//! relying on subjective `top`/`htop` observation. Bumping the ceiling
//! is intentional friction — if a real change needs more headroom,
//! prove it's bounded and update the constant with a why-comment.
//!
//! Sits in `core/tests/` so it has its own dedicated test binary; the
//! global allocator doesn't leak into other tests.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hiker_core::embed::{Embedder, MockEmbedder};
use hiker_core::indexer::{IndexJob, start_indexer};
use hiker_core::store::Store;
use hiker_core::vault::Vault;

// ---- Counting allocator -------------------------------------------------

struct CountingAllocator;

static CURRENT_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let new_cur = CURRENT_BYTES.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            // Lock-free peak update: only one CAS per allocation that
            // pushes the watermark.
            let mut peak = PEAK_BYTES.load(Ordering::Relaxed);
            while new_cur > peak {
                match PEAK_BYTES.compare_exchange_weak(
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
        CURRENT_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator;

fn current() -> usize {
    CURRENT_BYTES.load(Ordering::Relaxed)
}
fn peak() -> usize {
    PEAK_BYTES.load(Ordering::Relaxed)
}
fn reset_peak_to_current() {
    let cur = current();
    PEAK_BYTES.store(cur, Ordering::Relaxed);
}

// ---- Synthetic vault ----------------------------------------------------

/// Number of synthetic markdown files to write. Large enough to exercise
/// the per-file pipeline many times so a per-file leak shows up; small
/// enough that the test runs fast on CI.
const VAULT_FILES: usize = 256;
/// Approximate body size per synthetic file (bytes).
const FILE_BODY_BYTES: usize = 4 * 1024;

/// Hard ceiling on heap bytes *added* by the indexer pass over the
/// synthetic vault. Sized so a regression that retains per-file state
/// (e.g. forgets to drop file body, or stops trimming a cache) trips
/// the test within one CI run rather than waiting for production OOM.
///
/// Current observed peak on this fixture is ~80 KB; setting the
/// ceiling at 8 MiB leaves ~100× headroom for unrelated allocator
/// churn while still catching the "retain one MAX_FILE_BYTES (5 MiB)
/// body per file" class of bug. Bumping this is intentional friction
/// — if a real change needs more headroom, prove it's bounded and add
/// a why-comment.
const PEAK_CEILING_BYTES: usize = 8 * 1024 * 1024;

fn build_synthetic_vault(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    let body: String = "lorem ipsum dolor sit amet, consectetur adipiscing elit. "
        .repeat(FILE_BODY_BYTES / 56);
    for i in 0..VAULT_FILES {
        let path = root.join(format!("note-{i:04}.md"));
        let content = format!("# Note {i}\n\n{body}\n");
        std::fs::write(&path, content)?;
    }
    Ok(())
}

// ---- The test ------------------------------------------------------------

/// Catches regressions that grow heap proportional to vault size *after*
/// the indexer has finished. A correct implementation drops per-file
/// state once the file is upserted; the heap should settle close to
/// where it started before the scan.
#[test]
fn indexer_full_scan_stays_under_ceiling() {
    // Use a temp dir under the system tmp so the run cleans up.
    let tmp = std::env::temp_dir().join(format!(
        "hiker-heap-ceiling-{}-{}",
        std::process::id(),
        rand_suffix(),
    ));
    let vault_root = tmp.join("vault");
    let db_dir = tmp.join("db");
    std::fs::create_dir_all(&vault_root).unwrap();
    std::fs::create_dir_all(&db_dir).unwrap();
    let cleanup = TempCleanup(tmp.clone());

    build_synthetic_vault(&vault_root).expect("write synthetic vault");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async move {
        let store = Store::open(&db_dir).expect("open store");
        let vault = Vault::open(vault_root.clone()).expect("open vault");

        // Re-baseline the peak after setup so the assertion measures
        // the *indexer's* peak, not the test harness fixture cost.
        reset_peak_to_current();
        let baseline = current();
        let baseline_peak = peak();

        let handle = start_indexer(vault, store, || {
            Ok(Arc::new(MockEmbedder::new("ceiling-mock")) as Arc<dyn Embedder>)
        });
        handle
            .enqueue(IndexJob::FullScan { force: false })
            .await
            .expect("enqueue full scan");

        // Wait for drain. The synthetic vault is small (~256 files,
        // ~1 MiB) so this should be under 10s even on cold CPUs.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if Instant::now() > deadline {
                panic!(
                    "indexer did not drain in 30s — pending={} total={}",
                    handle.pending_paths().len(),
                    handle.status().total_notes,
                );
            }
            let pending = handle.pending_paths().len();
            let total = handle.status().total_notes;
            if pending == 0 && total as usize >= VAULT_FILES {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let post_scan = current();
        let post_scan_peak = peak();
        let added_peak = post_scan_peak.saturating_sub(baseline_peak);
        let leaked_residual = post_scan.saturating_sub(baseline);

        eprintln!(
            "heap_ceiling: baseline={baseline} bytes, post_scan={post_scan} bytes, \
             baseline_peak={baseline_peak} bytes, post_scan_peak={post_scan_peak} bytes, \
             added_peak={added_peak} bytes, leaked_residual={leaked_residual} bytes, \
             notes={}",
            handle.status().total_notes,
        );

        handle.shutdown().await;

        assert!(
            added_peak < PEAK_CEILING_BYTES,
            "indexer peak heap grew by {added_peak} bytes, ceiling is \
             {PEAK_CEILING_BYTES}. If a real change needs more headroom, prove \
             it's bounded and bump PEAK_CEILING_BYTES with a why-comment.",
        );
    });

    drop(cleanup);
}

// ---- Helpers ------------------------------------------------------------

struct TempCleanup(std::path::PathBuf);
impl Drop for TempCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rand_suffix() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}
