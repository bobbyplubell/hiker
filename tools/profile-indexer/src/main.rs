//! Heap-profile the indexer's full-scan path with `dhat`.
//!
//! Usage:
//!   cargo run --release -p profile-indexer -- /path/to/vault
//!
//! Writes `dhat-heap.json` in the current directory. Open in
//! <https://nnethercote.github.io/dh_view/dh_view.html> to navigate the
//! per-allocation-site breakdown.
//!
//! The binary runs against a `MockEmbedder` so the ONNX runtime (which
//! has its own arenas and would dominate the heap profile) doesn't
//! mask the indexer's own allocations. If you specifically need to
//! profile the embedder, swap in `FastembedEmbedder::load_default`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use hiker_core::embed::{Embedder, MockEmbedder};
use hiker_core::indexer::{IndexJob, start_indexer};
use hiker_core::store::Store;
use hiker_core::vault::Vault;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let vault_root: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("usage: profile-indexer <vault-path>"))?;
    let vault_root = vault_root
        .canonicalize()
        .with_context(|| format!("canonicalize {}", vault_root.display()))?;
    tracing::info!(vault = %vault_root.display(), "profile-indexer starting");

    // Use a temp dir for the index DB so we always start clean and
    // never pollute the vault. The vault is read-only from our POV.
    let db_dir = tempdir()?;
    let store = Store::open(db_dir.path())
        .context("open store in temp dir")?;
    let vault = Vault::open(vault_root.clone())?;

    // dhat profiler — heap snapshots from this point until `_profiler`
    // drops at end of `main`. dhat writes `dhat-heap.json` on drop.
    let _profiler = dhat::Profiler::new_heap();
    let scan_start = Instant::now();

    let handle = start_indexer(vault, store, || {
        Ok(Arc::new(MockEmbedder::new("profile-mock")) as Arc<dyn Embedder>)
    });

    // Kick off a full scan and wait for the indexer to drain.
    handle
        .enqueue(IndexJob::FullScan { force: false })
        .await
        .context("enqueue FullScan")?;

    // Poll the status watch until pending hits zero AND we've seen at
    // least one indexed event (so we don't bail out before the scan
    // even kicked off). 60s hard ceiling so the profile binary never
    // hangs in CI.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last_total = 0;
    loop {
        if Instant::now() > deadline {
            tracing::warn!("profile-indexer: 60s deadline hit; shutting down anyway");
            break;
        }
        let pending = handle.pending_paths().len();
        let total = handle.status().total_notes;
        if pending == 0 && total > 0 && total == last_total {
            // Two consecutive zero-pending polls with stable total —
            // scan is done.
            break;
        }
        last_total = total;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let final_total = handle.status().total_notes;
    let elapsed = scan_start.elapsed();
    tracing::info!(
        notes = final_total,
        elapsed_ms = elapsed.as_millis() as u64,
        "profile-indexer: scan complete",
    );

    handle.shutdown().await;
    // `_profiler` drops here → writes dhat-heap.json.
    Ok(())
}

/// Minimal tempdir without pulling in the `tempfile` crate as a build
/// dependency of the whole workspace just for one binary.
fn tempdir() -> Result<TempDir> {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = base.join(format!("profile-indexer-{pid}-{nanos}"));
    std::fs::create_dir_all(&path).context("mkdir profile tempdir")?;
    Ok(TempDir { path })
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
