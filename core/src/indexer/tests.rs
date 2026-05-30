use super::*;
use crate::embed::{MockEmbedder, Error as EmbedError};
use crate::store::dto::{new_id, NoteUpsert};
use std::fs;
use tempfile::tempdir;

fn mock_loader() -> impl FnOnce() -> Result<Arc<dyn Embedder>, EmbedError> + Send + 'static {
    || Ok(Arc::new(MockEmbedder::new("mock-v1")) as Arc<dyn Embedder>)
}

async fn await_event<F>(rx: &mut broadcast::Receiver<ProgressEvent>, pred: F) -> ProgressEvent
where
    F: Fn(&ProgressEvent) -> bool,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let ev = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("timed out waiting for progress event")
            .expect("progress channel closed");
        if pred(&ev) {
            return ev;
        }
    }
}

#[tokio::test]
async fn indexer_indexes_a_markdown_file() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("alpha.md"), b"# Alpha\n\nbody.\n").unwrap();
    let store = Store::open(dir.path()).unwrap();
    let handle = start(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
    let mut prog = handle.subscribe_progress();

    // Wait for ModelLoaded so the loader future has resolved.
    await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;

    handle.index_path("alpha.md").await.unwrap();
    await_event(&mut prog, |e| {
        matches!(e, ProgressEvent::Finished { path } if path == "alpha.md")
    })
    .await;

    // Reopen a reader Store and verify rows.
    let store2 = Store::open(dir.path()).unwrap();
    let note = store2.get_note_by_path("alpha.md").unwrap().unwrap();
    let chunks = store2.get_note_chunks(&note.id).unwrap();
    assert!(!chunks.is_empty());
}

#[tokio::test]
async fn unchanged_file_is_skipped_on_second_index() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("x.md"), b"# X\n\nbody.\n").unwrap();
    let store = Store::open(dir.path()).unwrap();
    let handle = start(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
    let mut prog = handle.subscribe_progress();

    await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;

    handle.index_path("x.md").await.unwrap();
    await_event(&mut prog, |e| matches!(e, ProgressEvent::Finished { .. })).await;

    handle.index_path("x.md").await.unwrap();
    let ev = await_event(&mut prog, |e| {
        matches!(e, ProgressEvent::Skipped { reason, .. } if reason == "unchanged")
            || matches!(e, ProgressEvent::Finished { .. })
    })
    .await;
    assert!(matches!(ev, ProgressEvent::Skipped { .. }));
}

#[tokio::test]
async fn force_reindex_bypasses_unchanged_short_circuit() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("y.md"), b"# Y\n\nbody.\n").unwrap();
    let store = Store::open(dir.path()).unwrap();
    let handle = start(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
    let mut prog = handle.subscribe_progress();

    await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;

    handle.index_path("y.md").await.unwrap();
    await_event(&mut prog, |e| matches!(e, ProgressEvent::Finished { .. })).await;

    // force = true → identical bytes still produce a Finished, not Skipped.
    handle
        .job_sender()
        .send(IndexJob::Upsert { rel_path: "y.md".into(), force: true })
        .await
        .unwrap();
    let ev = await_event(&mut prog, |e| {
        matches!(e, ProgressEvent::Finished { path, .. } if path == "y.md")
            || matches!(e, ProgressEvent::Skipped { path, .. } if path == "y.md")
    })
    .await;
    assert!(matches!(ev, ProgressEvent::Finished { .. }));
}

#[tokio::test]
async fn deleting_a_note_removes_it_from_the_index() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("doomed.md"), b"# x\n").unwrap();
    let store = Store::open(dir.path()).unwrap();
    let handle = start(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
    let mut prog = handle.subscribe_progress();

    await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;
    handle.index_path("doomed.md").await.unwrap();
    await_event(&mut prog, |e| matches!(e, ProgressEvent::Finished { .. })).await;

    handle
        .enqueue(IndexJob::Delete { rel_path: "doomed.md".into() })
        .await
        .unwrap();
    await_event(&mut prog, |e| matches!(e, ProgressEvent::Deleted { .. })).await;

    let store2 = Store::open(dir.path()).unwrap();
    assert!(store2.get_note_by_path("doomed.md").unwrap().is_none());
}

#[tokio::test]
async fn renaming_preserves_id() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("old.md"), b"# x\n").unwrap();
    let store = Store::open(dir.path()).unwrap();
    let handle = start(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
    let mut prog = handle.subscribe_progress();

    await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;
    handle.index_path("old.md").await.unwrap();
    await_event(&mut prog, |e| matches!(e, ProgressEvent::Finished { .. })).await;

    let store_check = Store::open(dir.path()).unwrap();
    let id_before = store_check
        .get_note_by_path("old.md")
        .unwrap()
        .unwrap()
        .id;
    drop(store_check);

    handle
        .enqueue(IndexJob::Rename {
            from: "old.md".into(),
            to: "new.md".into(),
        })
        .await
        .unwrap();
    await_event(&mut prog, |e| matches!(e, ProgressEvent::Renamed { .. })).await;

    let store_after = Store::open(dir.path()).unwrap();
    assert!(store_after.get_note_by_path("old.md").unwrap().is_none());
    let after = store_after.get_note_by_path("new.md").unwrap().unwrap();
    assert_eq!(after.id, id_before);
}

#[test]
fn full_scan_finds_md_files_and_skips_hiker_dir() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.md"), b"a").unwrap();
    fs::create_dir_all(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub/b.md"), b"b").unwrap();
    fs::write(dir.path().join("c.log"), b"not indexed").unwrap();
    // .hiker/ subtree must be skipped.
    fs::create_dir_all(dir.path().join(".hiker/refs")).unwrap();
    fs::write(dir.path().join(".hiker/refs/secret.md"), b"x").unwrap();

    let store = Store::open(dir.path()).unwrap();
    let jobs = run_full_scan(dir.path(), &store, false).unwrap();
    let upserts: Vec<&String> = jobs
        .iter()
        .filter_map(|j| match j {
            IndexJob::Upsert { rel_path, .. } => Some(rel_path),
            _ => None,
        })
        .collect();
    assert!(upserts.iter().any(|p| p.as_str() == "a.md"));
    assert!(upserts.iter().any(|p| p.as_str() == "sub/b.md"));
    assert!(!upserts.iter().any(|p| p.contains(".hiker")));
    assert!(!upserts.iter().any(|p| p.ends_with("c.log")));
}

#[test]
fn full_scan_emits_delete_for_missing_files() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("present.md"), b"x").unwrap();

    // Pre-populate the store with a note for a path that doesn't exist
    // on disk.
    let mut store = Store::open(dir.path()).unwrap();
    let id = new_id();
    store
        .upsert_note(&NoteUpsert {
            id: &id,
            path: "ghost.md",
            content_hash: "h",
            mtime: 0,
            size: 0,
            indexed_at: 0,
            embedder_version: "mock-v1",
            chunks: Vec::new(),
        })
        .unwrap();

    let jobs = run_full_scan(dir.path(), &store, false).unwrap();
    let deletes: Vec<&String> = jobs
        .iter()
        .filter_map(|j| match j {
            IndexJob::Delete { rel_path } => Some(rel_path),
            _ => None,
        })
        .collect();
    assert!(deletes.iter().any(|p| p.as_str() == "ghost.md"));
}

#[tokio::test]
async fn missing_file_during_upsert_is_treated_as_delete() {
    let dir = tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let handle = start(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
    let mut prog = handle.subscribe_progress();

    await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;
    // Upsert a path that doesn't exist on disk.
    handle.index_path("nope.md").await.unwrap();
    let ev = await_event(&mut prog, |e| {
        matches!(e, ProgressEvent::Skipped { .. } | ProgressEvent::Error { .. })
    })
    .await;
    // No panic, no error — just a skip with reason about missing-on-disk.
    assert!(matches!(ev, ProgressEvent::Skipped { .. }));
}

#[tokio::test]
async fn unsupported_extensions_are_skipped() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.bin"), b"x").unwrap();
    let store = Store::open(dir.path()).unwrap();
    let handle = start(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
    let mut prog = handle.subscribe_progress();

    await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;
    handle.index_path("a.bin").await.unwrap();
    let ev = await_event(&mut prog, |e| matches!(e, ProgressEvent::Skipped { .. })).await;
    if let ProgressEvent::Skipped { reason, .. } = ev {
        assert_eq!(reason, "unsupported extension");
    }
}

// status: trail-waypoints-derived-table
#[tokio::test]
async fn ingesting_trail_doc_and_waypoint_populates_derived_table() {
    let dir = tempdir().unwrap();
    let trail_id = "01HTRAILTEST";
    let waypoint_id = "01HWPTEST";
    let source_id = "01HSRCTEST"; // not used directly; source uses path-based lookup
    let _ = source_id;

    // Write the source note first so the indexer assigns an id we can
    // look up on the waypoint's source_id column.
    std::fs::create_dir_all(dir.path().join("research")).unwrap();
    std::fs::write(
        dir.path().join("research/raptor.md"),
        "# Raptor\n\nbody.\n",
    )
    .unwrap();

    // Trail-doc. status: trail-path-references (no `hiker.id`, no id half
    // on the waypoint entry — the trail's storage key is its op-log
    // doc_id, looked up below).
    std::fs::create_dir_all(dir.path().join("trails")).unwrap();
    let trail_doc = format!(
        "---\nhiker:\n  kind: trail\n  waypoints:\n    - path: .hiker/trails/{trail_id}/waypoints/0001--raptor.md\n---\nbody\n"
    );
    std::fs::write(dir.path().join("trails/my-trail.md"), trail_doc).unwrap();

    // Waypoint-note. status: waypoint-note-shape — references and
    // in_trail are path-only.
    let waypoint_dir = dir
        .path()
        .join(format!(".hiker/trails/{trail_id}/waypoints"));
    std::fs::create_dir_all(&waypoint_dir).unwrap();
    let wp = "---\nhiker:\n  kind: waypoint\n  references:\n    path: research/raptor.md\n  in_trail:\n    path: trails/my-trail.md\n---\n".to_string();
    std::fs::write(waypoint_dir.join("0001--raptor.md"), wp).unwrap();

    let store = Store::open(dir.path()).unwrap();
    let vault = crate::vault::Vault::open(dir.path()).unwrap();
    // status: store-id-from-oplog / op-log-bootstraps-first
    // The trail / waypoint derived-table re-derive reads the op-log's
    // doc_id mapping; bootstrap + attach before any ingest. Bootstrap's
    // walker skips `.hiker/` so the waypoint-note under the carved-out
    // `.hiker/trails/` dir won't auto-seed — seed it explicitly via
    // `doc_id_or_seed`, the same shape `create_trail` uses.
    let oplog = std::sync::Arc::new(crate::oplog::OpLog::open(dir.path()).unwrap());
    crate::ops::op_writes::bootstrap(&vault, &oplog).unwrap();
    let waypoint_rel = format!(
        ".hiker/trails/{trail_id}/waypoints/0001--raptor.md"
    );
    crate::ops::op_writes::doc_id_or_seed(&oplog, &vault, &waypoint_rel, "").unwrap();
    let handle = start(vault, store, mock_loader());
    handle.attach_oplog(oplog.clone());
    let mut prog = handle.subscribe_progress();
    await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;

    // Index source first so its id is available when the waypoint
    // ingests; then trail-doc; then waypoint-note.
    handle.index_path("research/raptor.md").await.unwrap();
    await_event(&mut prog, |e| {
        matches!(e, ProgressEvent::Finished { path } if path == "research/raptor.md")
    })
    .await;

    handle
        .index_path(format!(
            ".hiker/trails/{trail_id}/waypoints/0001--raptor.md"
        ))
        .await
        .unwrap();
    await_event(&mut prog, |e| {
        matches!(e, ProgressEvent::Finished { path }
            if path.ends_with("0001--raptor.md"))
    })
    .await;

    // Trail-doc ingested AFTER the waypoint-note so the depth-first
    // walk sees the per-row `source_path` and produces canonical
    // `parent_waypoint_id` + `tree_path` values. Mirrors
    // `append_waypoint`'s waypoint-then-trail-doc enqueue order.
    handle.index_path("trails/my-trail.md").await.unwrap();
    await_event(&mut prog, |e| {
        matches!(e, ProgressEvent::Finished { path } if path == "trails/my-trail.md")
    })
    .await;

    // Verify derived rows. status: store-id-from-oplog —
    // `trail_id` / `waypoint_id` are now the op-log doc_ids for the
    // trail-doc / waypoint-note paths, not the legacy `hiker.id` stamps.
    let trail_doc_id = oplog
        .doc_id_for_path("trails/my-trail.md")
        .unwrap()
        .expect("trail-doc seeded");
    let waypoint_doc_id = oplog
        .doc_id_for_path(&format!(
            ".hiker/trails/{trail_id}/waypoints/0001--raptor.md"
        ))
        .unwrap()
        .expect("waypoint seeded");
    let store2 = Store::open(dir.path()).unwrap();
    let waypoints = store2.waypoints_of(&trail_doc_id).unwrap();
    assert_eq!(waypoints.len(), 1);
    assert_eq!(waypoints[0].waypoint_id, waypoint_doc_id);
    assert_eq!(waypoints[0].tree_path, "1");
    assert_eq!(waypoints[0].source_path, "research/raptor.md");
    assert!(waypoints[0].parent_waypoint_id.is_none());
    // source_id was looked up via the just-ingested source note.
    assert!(waypoints[0].source_id.is_some());

    let containing = store2.trails_containing_note("research/raptor.md").unwrap();
    assert_eq!(containing.len(), 1);
    assert_eq!(containing[0].trail_id, trail_doc_id);
    let _ = waypoint_id;
}

#[tokio::test]
async fn txt_files_are_indexed() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("note.txt"), b"first paragraph.\n\nsecond paragraph.\n").unwrap();
    let store = Store::open(dir.path()).unwrap();
    let handle = start(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
    let mut prog = handle.subscribe_progress();

    await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;
    handle.index_path("note.txt").await.unwrap();
    await_event(&mut prog, |e| {
        matches!(e, ProgressEvent::Finished { path } if path == "note.txt")
    })
    .await;

    let store2 = Store::open(dir.path()).unwrap();
    let note = store2.get_note_by_path("note.txt").unwrap().unwrap();
    let chunks = store2.get_note_chunks(&note.id).unwrap();
    assert!(!chunks.is_empty());
    assert!(chunks.iter().any(|c| c.text.contains("first paragraph")));
    assert!(chunks.iter().any(|c| c.text.contains("second paragraph")));
}
