//! End-to-end tests for the append cursor (`trail-append-cursor`):
//! `set_append_cursor` set / reset / stale-fallback, `append_waypoint`
//! honoring the cursor as the parent when no explicit parent is given,
//! and `remove_waypoint` clearing the cursor when it cascades through the
//! cursor's waypoint. The cursor lives in the trail-doc's
//! `hiker.append_under` frontmatter, so each test asserts against the
//! parsed trail-doc on disk.
//!
//! These ops write the trail-doc synchronously before returning (and seed
//! the layered-doc doc_id in `create_trail`), so the tests assert directly
//! against the on-disk frontmatter. A live indexer runs only to drain the
//! `IndexJob` channel the ops enqueue onto — its derived tables aren't
//! read here (unlike the rename test, which needs `trail_waypoints`).

use std::sync::Arc;

use tempfile::TempDir;

use crate::config::sections::TrailsConfig;
use crate::embed::{Embedder, Error as EmbedError};
use crate::indexer;
use crate::editing::LayeredDoc;
use crate::store::Store;
use crate::trails::ops::{
    append_waypoint, create_trail, remove_waypoint, set_append_cursor, AppendWaypointArgs,
};
use crate::trails::parse_trail_doc;
use crate::trash::Trash;
use crate::vault::Vault;
use crate::watcher::Watcher;

struct ZeroEmbedder;
impl Embedder for ZeroEmbedder {
    fn embed_batch(&self, batch: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(batch.iter().map(|_| vec![0.0; 384]).collect())
    }
    fn version(&self) -> &str {
        "zero-test"
    }
    fn dim(&self) -> usize {
        384
    }
}

/// Shared fixture: a vault with a running (channel-draining) indexer, a
/// created trail whose layered-doc doc_id is seeded, and three source notes on
/// disk to point waypoints at.
struct Fixture {
    _td: TempDir,
    vault: Vault,
    watcher: Watcher,
    trash: Trash,
    layered: Arc<LayeredDoc>,
    idx: indexer::Handle,
    trail_doc_rel: String,
}

async fn setup() -> Fixture {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let watcher = Watcher::start(td.path()).unwrap();
    let trash = Trash::open(td.path());
    let store = Store::open(td.path()).unwrap();
    let layered = Arc::new(LayeredDoc::open(td.path()).unwrap());
    let idx = indexer::start(vault.clone(), store, || {
        Ok(Arc::new(ZeroEmbedder) as Arc<dyn Embedder>)
    });
    idx.attach_layered(layered.clone());

    // Source notes for waypoints — written to disk so the waypoint append
    // has a real path to reference (the append never reads their bodies).
    std::fs::create_dir_all(td.path().join("research")).unwrap();
    for name in ["a", "b", "c"] {
        std::fs::write(td.path().join(format!("research/{name}.md")), "# x\n").unwrap();
    }

    let cfg = TrailsConfig::default();
    let created = create_trail(&watcher, &idx.job_sender(), &layered, &vault, &cfg, "t")
        .await
        .unwrap();

    Fixture {
        _td: td,
        vault,
        watcher,
        trash,
        layered,
        idx,
        trail_doc_rel: created.trail_doc_rel,
    }
}

impl Fixture {
    /// Append `source_rel` under the trail-doc's cursor / root tail (no
    /// explicit parent) and return the new waypoint path.
    async fn append(&self, source_rel: &str) -> String {
        append_waypoint(AppendWaypointArgs {
            watcher: &self.watcher,
            jobs: &self.idx.job_sender(),
            log: &self.layered,
            vault: &self.vault,
            trail_doc_rel: &self.trail_doc_rel,
            source_rel,
            parent_waypoint_path: None,
            annotation: None,
        })
        .await
        .unwrap()
        .waypoint_rel
    }

    async fn set_cursor(&self, waypoint_path: Option<&str>) {
        set_append_cursor(
            &self.watcher,
            &self.idx.job_sender(),
            &self.vault,
            &self.trail_doc_rel,
            waypoint_path,
        )
        .await
        .unwrap();
    }

    fn cursor(&self) -> Option<String> {
        let src = self.vault.read_file(&self.trail_doc_rel).unwrap();
        parse_trail_doc(&src).unwrap().append_under
    }
}

// status: trail-append-cursor
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_cursor_writes_append_under() {
    let fx = setup().await;
    let wp = fx.append("research/a.md").await;
    assert_eq!(fx.cursor(), None);

    fx.set_cursor(Some(&wp)).await;
    assert_eq!(fx.cursor().as_deref(), Some(wp.as_str()));

    fx.idx.shutdown().await;
}

// status: trail-append-cursor
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_cursor_to_null_clears_append_under() {
    let fx = setup().await;
    let wp = fx.append("research/a.md").await;
    fx.set_cursor(Some(&wp)).await;
    assert_eq!(fx.cursor().as_deref(), Some(wp.as_str()));

    fx.set_cursor(None).await;
    assert_eq!(fx.cursor(), None);

    fx.idx.shutdown().await;
}

// status: trail-append-cursor
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_cursor_target_warns_and_nulls() {
    let fx = setup().await;
    fx.append("research/a.md").await;

    // A path that isn't a waypoint in the tree must not be written; the
    // cursor stays None and the call still succeeds (no error).
    fx.set_cursor(Some("trails/t/ghost--ZZZZZZ.md")).await;
    assert_eq!(fx.cursor(), None);

    fx.idx.shutdown().await;
}

// status: trail-append-cursor
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn append_lands_under_cursor_and_cursor_stays_put() {
    let fx = setup().await;
    let parent = fx.append("research/a.md").await;
    fx.set_cursor(Some(&parent)).await;

    // Appending with no explicit parent consults the cursor: the new
    // waypoint becomes a child of `parent`.
    let child = fx.append("research/b.md").await;
    let src = fx.vault.read_file(&fx.trail_doc_rel).unwrap();
    let fm = parse_trail_doc(&src).unwrap();
    assert_eq!(fm.waypoints.len(), 1, "child should nest, not sit at root");
    assert_eq!(fm.waypoints[0].path, parent);
    assert_eq!(fm.waypoints[0].waypoints.len(), 1);
    assert_eq!(fm.waypoints[0].waypoints[0].path, child);

    // The append did not move the cursor — it still points at `parent`.
    assert_eq!(fx.cursor().as_deref(), Some(parent.as_str()));

    fx.idx.shutdown().await;
}

// status: trail-append-cursor
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_resets_when_removed_waypoint_cascades_through_it() {
    let fx = setup().await;
    let parent = fx.append("research/a.md").await;
    // Branch a child under `parent`, then point the cursor at the child.
    fx.set_cursor(Some(&parent)).await;
    let child = fx.append("research/b.md").await;
    fx.set_cursor(Some(&child)).await;
    assert_eq!(fx.cursor().as_deref(), Some(child.as_str()));

    // Removing `parent` cascades through `child` (the cursor's waypoint),
    // so the cursor resets to None in the same trail-doc rewrite.
    remove_waypoint(
        &fx.watcher,
        &fx.idx.job_sender(),
        &fx.vault,
        &fx.trash,
        &fx.trail_doc_rel,
        &parent,
    )
    .await
    .unwrap();
    assert_eq!(fx.cursor(), None);

    fx.idx.shutdown().await;
}
