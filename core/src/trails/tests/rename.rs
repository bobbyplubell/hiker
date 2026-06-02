//! End-to-end test: renaming a trail-doc moves its visible companion
//! folder of waypoint-notes (one `move_note` op) and rewrites the
//! cross-references — each waypoint's `hiker.in_trail` and the trail-doc's
//! `hiker.waypoints[].path` follow the rename
//! (`trail-storage-layout`, `note-companion-folder`).

use std::sync::Arc;

use tempfile::TempDir;
use tokio::sync::broadcast;

use crate::config::sections::TrailsConfig;
use crate::embed::{Embedder, Error as EmbedError};
use crate::indexer::{self, ProgressEvent};
use crate::oplog::OpLog;
use crate::store::Store;
use crate::trails::ops::{append_waypoint, create_trail, AppendWaypointArgs};
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

async fn await_finished(rx: &mut broadcast::Receiver<ProgressEvent>, path_suffix: &str) {
    // Hang-guard only — this is an end-to-end test that waits on three
    // sequential async indexer `Finished` events. Under `cargo test
    // --workspace` the indexer task competes with every other crate's tests
    // for CPU, so a tight deadline trips falsely while the work is merely
    // starved (the test passes comfortably in isolation). Size the guard for
    // loaded parallel runs; it bounds a genuine hang, it does not assert speed.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let ev = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("timed out waiting for progress")
            .expect("progress channel closed");
        if matches!(&ev, ProgressEvent::Finished { path } if path.ends_with(path_suffix)) {
            return;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_trail_doc_moves_companion_folder_and_rewrites_refs() {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let oplog = Arc::new(OpLog::open(td.path()).unwrap());
    let idx = indexer::start(vault.clone(), store, || {
        Ok(Arc::new(ZeroEmbedder) as Arc<dyn Embedder>)
    });
    idx.attach_oplog(oplog.clone());
    idx.attach_watcher(Arc::new(Watcher::start(td.path()).unwrap()));
    let mut prog = idx.subscribe_progress();

    // A source note to point a waypoint at.
    std::fs::create_dir_all(td.path().join("research")).unwrap();
    std::fs::write(td.path().join("research/raptor.md"), "# Raptor\n").unwrap();
    idx.index_path("research/raptor.md").await.unwrap();
    await_finished(&mut prog, "research/raptor.md").await;

    // Create a trail + append a waypoint (lazily creates the companion
    // folder at `trails/my-trail/`).
    let cfg = TrailsConfig::default();
    let created = create_trail(
        &watcher,
        &idx.job_sender(),
        &oplog,
        &vault,
        &cfg,
        "my-trail",
        false,
    )
    .await
    .unwrap();
    assert_eq!(created.trail_doc_rel, "trails/my-trail.md");

    let appended = append_waypoint(AppendWaypointArgs {
        watcher: &watcher,
        jobs: &idx.job_sender(),
        log: &oplog,
        vault: &vault,
        trail_doc_rel: "trails/my-trail.md",
        source_rel: "research/raptor.md",
        parent_waypoint_path: None,
        annotation: None,
    })
    .await
    .unwrap();
    let wp_rel = appended.waypoint_rel;
    assert!(wp_rel.starts_with("trails/my-trail/"));
    assert!(td.path().join(&wp_rel).exists());

    // Wait for the waypoint-note + trail-doc to ingest so the derived
    // `trail_waypoints` table is populated before the rename (the trail-doc
    // rewrite reads it to find the waypoints whose `in_trail` to update).
    await_finished(&mut prog, &wp_rel).await;
    await_finished(&mut prog, "trails/my-trail.md").await;

    // Rename the trail-doc. The companion folder + waypoint must follow.
    crate::ops::file::move_note(
        &watcher,
        &idx.job_sender(),
        "trails/my-trail.md",
        "trails/renamed.md",
    )
    .await
    .unwrap();

    assert!(!td.path().join("trails/my-trail.md").exists());
    assert!(!td.path().join("trails/my-trail").exists());
    assert!(td.path().join("trails/renamed.md").exists());
    let new_wp_rel = wp_rel.replacen("trails/my-trail/", "trails/renamed/", 1);
    assert!(
        td.path().join(&new_wp_rel).exists(),
        "waypoint should have moved into the renamed companion folder"
    );

    // The waypoint's `in_trail` now points at the renamed trail-doc.
    let wp_src = std::fs::read_to_string(td.path().join(&new_wp_rel)).unwrap();
    let wp_fm = crate::trails::parse_waypoint(&wp_src).unwrap();
    assert_eq!(wp_fm.in_trail, "trails/renamed.md");

    // The trail-doc's waypoint entry now points at the moved waypoint path.
    let trail_src = std::fs::read_to_string(td.path().join("trails/renamed.md")).unwrap();
    let trail_fm = crate::trails::parse_trail_doc(&trail_src).unwrap();
    assert_eq!(trail_fm.waypoints.len(), 1);
    assert_eq!(trail_fm.waypoints[0].path, new_wp_rel);

    idx.shutdown().await;
}
