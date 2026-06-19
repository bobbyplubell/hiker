use super::*;
use crate::embed::{MockEmbedder, Error as EmbedError};
use crate::store::dto::NoteUpsert;
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
    let chunks = store2.get_note_chunks(&note.path).unwrap();
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
async fn file_with_conflict_marker_text_is_indexed_normally() {
    // status: sync-unified-conflict-surface
    // Indexing is NOT gated on conflict-marker text: a legitimate file that merely
    // contains `<<<<<<< / ======= / >>>>>>>` lines (a tutorial, a pasted git
    // conflict, a bug report) is indexed normally, not skipped. The
    // `has_unresolved_conflicts` predicate still recognizes the text — it's a
    // building block for the future in-app conflict surface, not an index gate.
    let dir = tempdir().unwrap();
    let with_markers =
        "intro\n<<<<<<< ours\nMINE\n=======\nTHEIRS\n>>>>>>> theirs\nbody.\n";
    assert!(crate::merge::has_unresolved_conflicts(with_markers));
    fs::write(dir.path().join("c.md"), with_markers.as_bytes()).unwrap();
    let store = Store::open(dir.path()).unwrap();
    let handle = start(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
    let mut prog = handle.subscribe_progress();
    await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;

    handle.index_path("c.md").await.unwrap();
    await_event(&mut prog, |e| {
        matches!(e, ProgressEvent::Finished { path } if path == "c.md")
    })
    .await;
    let store2 = Store::open(dir.path()).unwrap();
    let note = store2.get_note_by_path("c.md").unwrap().unwrap();
    assert!(!note.skipped, "marker-bearing file should index, not be skipped");
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
    let hash_before = store_check
        .get_note_by_path("old.md")
        .unwrap()
        .unwrap()
        .content_hash;
    drop(store_check);

    handle
        .enqueue(IndexJob::Rename {
            from: "old.md".into(),
            to: "new.md".into(),
        })
        .await
        .unwrap();
    await_event(&mut prog, |e| matches!(e, ProgressEvent::Renamed { .. })).await;

    // Under path-as-identity the note's identity moves with its path; the row
    // re-keys to the new path and the content (hash) is unchanged.
    let store_after = Store::open(dir.path()).unwrap();
    assert!(store_after.get_note_by_path("old.md").unwrap().is_none());
    let after = store_after.get_note_by_path("new.md").unwrap().unwrap();
    assert_eq!(after.path, "new.md");
    assert_eq!(after.content_hash, hash_before);
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
    store
        .upsert_note(&NoteUpsert {
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

    // Write the source note first so the indexer records its rel-path on
    // the waypoint's source_path column.
    std::fs::create_dir_all(dir.path().join("research")).unwrap();
    std::fs::write(
        dir.path().join("research/raptor.md"),
        "# Raptor\n\nbody.\n",
    )
    .unwrap();

    // Trail-doc + waypoint in the trail-doc's *visible* companion folder
    // (`trails/my-trail/`, per note-companion-folder). status:
    // trail-path-references — no `hiker.id`; the trail's storage key is its
    // layered-doc doc_id, looked up below.
    std::fs::create_dir_all(dir.path().join("trails/my-trail")).unwrap();
    let trail_doc =
        "---\nhiker:\n  kind: trail\n  waypoints:\n    - path: trails/my-trail/0001--raptor.md\n---\nbody\n";
    std::fs::write(dir.path().join("trails/my-trail.md"), trail_doc).unwrap();

    // Waypoint-note. status: waypoint-note-shape — references and
    // in_trail are path-only.
    let wp = "---\nhiker:\n  kind: waypoint\n  references:\n    path: research/raptor.md\n  in_trail:\n    path: trails/my-trail.md\n---\n".to_string();
    std::fs::write(dir.path().join("trails/my-trail/0001--raptor.md"), wp).unwrap();

    let store = Store::open(dir.path()).unwrap();
    let vault = crate::vault::Vault::open(dir.path()).unwrap();
    // status: store-path-is-identity / op-log-bootstraps-first
    // The trail / waypoint derived-table re-derive reads the layered doc's
    // doc_id mapping; bootstrap + attach before any ingest. The waypoint
    // now lives in the visible companion folder, so the main bootstrap pass
    // seeds it — no explicit `.hiker/` seed needed.
    let layered = std::sync::Arc::new(crate::editing::LayeredDoc::open(dir.path()).unwrap());
    crate::ops::op_writes::bootstrap(&vault, &layered).unwrap();
    let waypoint_rel = "trails/my-trail/0001--raptor.md".to_string();
    let handle = start(vault, store, mock_loader());
    handle.attach_layered(layered.clone());
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
        .index_path(waypoint_rel.clone())
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

    // Verify derived rows. status: store-path-is-identity —
    // `trail_id` / `waypoint_id` are now the layered-doc doc_ids for the
    // trail-doc / waypoint-note paths, not the legacy `hiker.id` stamps.
    let trail_doc_id = layered
        .doc_id_for_path("trails/my-trail.md")
        .unwrap()
        .expect("trail-doc seeded");
    let waypoint_doc_id = layered
        .doc_id_for_path(&waypoint_rel)
        .unwrap()
        .expect("waypoint seeded");
    let store2 = Store::open(dir.path()).unwrap();
    let waypoints = store2.waypoints_of(&trail_doc_id).unwrap();
    assert_eq!(waypoints.len(), 1);
    assert_eq!(waypoints[0].waypoint_id, waypoint_doc_id);
    assert_eq!(waypoints[0].tree_path, "1");
    assert_eq!(waypoints[0].source_path, "research/raptor.md");
    assert!(waypoints[0].parent_waypoint_id.is_none());

    let containing = store2.trails_containing_note("research/raptor.md").unwrap();
    assert_eq!(containing.len(), 1);
    assert_eq!(containing[0].trail_id, trail_doc_id);
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
    let chunks = store2.get_note_chunks(&note.path).unwrap();
    assert!(!chunks.is_empty());
    assert!(chunks.iter().any(|c| c.text.contains("first paragraph")));
    assert!(chunks.iter().any(|c| c.text.contains("second paragraph")));
}

/// Ingesting a sprint board-doc derives `board_cards` rows exactly like a
/// plain board once the kind registry is attached — the indexer's
/// `update_board_cards_if_relevant` is one of the three registry-aware
/// parse-gate callers (`sprint-board-subtype`). An unregistered board-like
/// pretender stays inert.
#[tokio::test]
async fn sprint_board_doc_ingest_derives_board_cards() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("boards")).unwrap();
    fs::write(dir.path().join("story.md"), b"the story\n").unwrap();
    let sprint = "---\nhiker:\n  kind: sprint\n  columns:\n    - name: Doing\n      cards:\n        - { path: story.md }\n---\n";
    fs::write(dir.path().join("boards/s1.md"), sprint).unwrap();
    let pretender = "---\nhiker:\n  kind: zettel\n  columns:\n    - name: Doing\n      cards:\n        - { path: story.md }\n---\n";
    fs::write(dir.path().join("boards/z.md"), pretender).unwrap();

    let store = Store::open(dir.path()).unwrap();
    let vault = crate::vault::Vault::open(dir.path()).unwrap();
    let layered = std::sync::Arc::new(crate::editing::LayeredDoc::open(dir.path()).unwrap());
    crate::ops::op_writes::bootstrap(&vault, &layered).unwrap();
    let handle = start(vault, store, mock_loader());
    handle.attach_layered(layered.clone());
    handle.attach_kind_registry(std::sync::Arc::new(crate::kinds::builtin_registry()));
    let mut prog = handle.subscribe_progress();
    await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;

    for p in ["story.md", "boards/s1.md", "boards/z.md"] {
        handle.index_path(p).await.unwrap();
        await_event(&mut prog, |e| {
            matches!(e, ProgressEvent::Finished { path } if path == p)
        })
        .await;
    }

    let reader = Store::open(dir.path()).unwrap();
    let containing = reader.boards_containing_note("story.md").unwrap();
    assert_eq!(containing.len(), 1, "sprint rows derived; pretender inert");
    assert_eq!(containing[0].board_path, "boards/s1.md");
    assert_eq!(containing[0].column_name, "Doing");
}

/// Ingesting a note whose `hiker.kind` names a registered kind re-derives
/// its lenient-validation problems into the store; a clean note (or one
/// with an unregistered kind) keeps no rows. `kind-lenient-validation` —
/// the write is never blocked, the file never rewritten.
#[tokio::test]
async fn ingest_derives_lenient_validation_problems() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("bad-story.md"),
        b"---\nhiker:\n  kind: story\npriority: soon\ndue: someday\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("good-story.md"),
        b"---\nhiker:\n  kind: story\npriority: 2\ndue: 2026-07-01\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("unregistered.md"),
        b"---\nhiker:\n  kind: zettel\npriority: soon\n---\nbody\n",
    )
    .unwrap();
    let store = Store::open(dir.path()).unwrap();
    let handle = start(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
    // Use the built-in PM set (story carries number/date fields).
    handle.attach_kind_registry(std::sync::Arc::new(crate::kinds::builtin_registry()));
    let mut prog = handle.subscribe_progress();
    await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;

    for p in ["bad-story.md", "good-story.md", "unregistered.md"] {
        handle.index_path(p).await.unwrap();
        await_event(&mut prog, |e| {
            matches!(e, ProgressEvent::Finished { path } if path == p)
        })
        .await;
    }

    let reader = Store::open(dir.path()).unwrap();
    // The file is untouched on disk (never rewritten)...
    let raw_body = fs::read_to_string(dir.path().join("bad-story.md")).unwrap();
    assert!(raw_body.contains("priority: soon"));
    // ...and carries one problem per violated primitive.
    let problems = reader.note_problems("bad-story.md").unwrap();
    let fields: Vec<&str> = problems.iter().map(|p| p.field.as_str()).collect();
    assert_eq!(fields, vec!["due", "priority"]);
    assert!(reader.note_problems("good-story.md").unwrap().is_empty());
    // Unregistered `hiker.kind` values stay inert — never validated.
    assert!(reader.note_problems("unregistered.md").unwrap().is_empty());
}

/// Ingesting a list-like note (`hiker.kind: epic` from the registry)
/// derives `list_refs` rows in ref order; an unregistered list-like
/// pretender stays inert; deleting the list-doc clears its rows — the
/// `board_cards` lifecycle exactly (`pm-epic-derived-table`).
#[tokio::test]
async fn list_doc_ingest_derives_and_delete_clears_list_refs() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("epics")).unwrap();
    let epic =
        "---\nhiker:\n  kind: epic\n  refs:\n    - { path: b.md }\n    - { path: a.md }\n---\nframing\n";
    fs::write(dir.path().join("epics/e1.md"), epic).unwrap();
    let pretender = epic.replace("kind: epic", "kind: roadmap");
    fs::write(dir.path().join("epics/z.md"), pretender).unwrap();

    let store = Store::open(dir.path()).unwrap();
    let handle = start(crate::vault::Vault::open(dir.path()).unwrap(), store, mock_loader());
    handle.attach_kind_registry(std::sync::Arc::new(crate::kinds::builtin_registry()));
    let mut prog = handle.subscribe_progress();
    await_event(&mut prog, |e| matches!(e, ProgressEvent::ModelLoaded)).await;

    for p in ["epics/e1.md", "epics/z.md"] {
        handle.index_path(p).await.unwrap();
        await_event(&mut prog, |e| {
            matches!(e, ProgressEvent::Finished { path } if path == p)
        })
        .await;
    }

    let reader = Store::open(dir.path()).unwrap();
    let members = reader.members_of("epics/e1.md").unwrap();
    assert_eq!(
        members.iter().map(|m| m.member_path.as_str()).collect::<Vec<_>>(),
        ["b.md", "a.md"],
        "rows derived in ref order"
    );
    assert!(
        reader.members_of("epics/z.md").unwrap().is_empty(),
        "unregistered kind derives nothing"
    );
    let containing = reader.lists_containing_note("a.md").unwrap();
    assert_eq!(containing.len(), 1);
    assert_eq!(containing[0].list_path, "epics/e1.md");

    // Delete clears the list's derived rows.
    handle
        .enqueue(IndexJob::Delete { rel_path: "epics/e1.md".into() })
        .await
        .unwrap();
    await_event(&mut prog, |e| {
        matches!(e, ProgressEvent::Deleted { path } if path == "epics/e1.md")
    })
    .await;
    let reader = Store::open(dir.path()).unwrap();
    assert!(reader.members_of("epics/e1.md").unwrap().is_empty());
}
