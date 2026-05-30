use rusqlite::{params, Connection};

use super::{Store, DEFAULT_EMBED_DIM, SCHEMA_VERSION};
use crate::chunker::Chunk;
use crate::store::error::Error;
use crate::store::dto::{
    new_id, MetaEntry, MetaFilter, NoteOrder, NoteQuery, NoteUpsert, OrderDir, WaypointRow,
};
use crate::test_helpers::test_store as fresh_store;
use tempfile::tempdir;

fn unit_vec(seed: f32) -> Vec<f32> {
    // Deterministic, distinct vectors. Each entry differs from the next by
    // a fixed offset; not unit-norm but fine for L2 KNN tests.
    (0..DEFAULT_EMBED_DIM).map(|i| seed + i as f32 * 0.001).collect()
}

fn mk_chunk(idx: u32, text: &str) -> Chunk {
    Chunk {
        index: idx,
        byte_start: 0,
        byte_end: text.len(),
        text: text.to_string(),
        heading_path: None,
    }
}

#[test]
fn open_creates_db_and_schema() {
    let (_dir, store) = fresh_store();
    assert!(store.db_path().exists());
    // Idempotent re-open works on the same path.
    let _again = Store::open(_dir.path()).unwrap();
}

#[test]
fn version_mismatch_fails_loud() {
    let dir = tempdir().unwrap();
    let _ = Store::open(dir.path()).unwrap();
    // Corrupt the user_version to simulate a future db.
    let conn = Connection::open(dir.path().join(".hiker/index.db")).unwrap();
    conn.pragma_update(None, "user_version", 99).unwrap();
    drop(conn);
    match Store::open(dir.path()) {
        Err(Error::VersionMismatch { found, expected }) => {
            assert_eq!(found, 99);
            assert_eq!(expected, SCHEMA_VERSION);
        }
        Err(e) => panic!("expected VersionMismatch, got {e:?}"),
        Ok(_) => panic!("expected VersionMismatch, got Ok(Store)"),
    }
}

#[test]
fn upsert_then_read() {
    let (_dir, mut store) = fresh_store();
    let id = new_id();
    store
        .upsert_note(&NoteUpsert {
            id: &id,
            path: "alpha.md",
            content_hash: "abc",
            mtime: 100,
            size: 42,
            indexed_at: 200,
            embedder_version: "test",
            chunks: vec![
                (mk_chunk(0, "hello world"), unit_vec(0.0)),
                (mk_chunk(1, "second chunk"), unit_vec(1.0)),
            ],
        })
        .unwrap();

    let note = store.get_note_by_path("alpha.md").unwrap().unwrap();
    assert_eq!(note.id, id);
    assert_eq!(note.size, 42);

    let chunks = store.get_note_chunks(&id).unwrap();
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].chunk_index, 0);
    assert_eq!(chunks[1].text, "second chunk");

    assert_eq!(
        store
            .get_note_by_path("alpha.md")
            .unwrap()
            .map(|r| r.id),
        Some(id.clone())
    );
}

#[test]
fn note_embedding_computes_byte_weighted_mean_on_upsert() {
    let (_dir, mut store) = fresh_store();
    let id = new_id();

    // Two chunks; second chunk is 3× the byte-length so it dominates
    // the weighted mean.
    let mut c0 = mk_chunk(0, "");
    c0.byte_start = 0;
    c0.byte_end = 10;
    let mut c1 = mk_chunk(1, "");
    c1.byte_start = 10;
    c1.byte_end = 40;
    // Make every dim of c0's embedding 1.0 and c1's 5.0; weighted
    // mean = (10*1 + 30*5) / 40 = 4.0.
    let e0 = vec![1.0f32; DEFAULT_EMBED_DIM];
    let e1 = vec![5.0f32; DEFAULT_EMBED_DIM];

    store
        .upsert_note(&NoteUpsert {
            id: &id,
            path: "n.md",
            content_hash: "h1",
            mtime: 0,
            size: 40,
            indexed_at: 0,
            embedder_version: "test",
            chunks: vec![(c0, e0), (c1, e1)],
        })
        .unwrap();

    // Fresh upsert populates note_embedding inline with the byte-weighted pool.
    let cached = store
        .note_embedding_for_path("n.md")
        .unwrap()
        .expect("populated by upsert");
    assert_eq!(cached.len(), DEFAULT_EMBED_DIM);
    for v in &cached {
        assert!((v - 4.0).abs() < 1e-4, "expected ~4.0, got {v}");
    }

    // Re-upserting with new chunks refreshes the pool.
    store
        .upsert_note(&NoteUpsert {
            id: &id,
            path: "n.md",
            content_hash: "h2",
            mtime: 1,
            size: 5,
            indexed_at: 1,
            embedder_version: "test",
            chunks: vec![(mk_chunk(0, "x"), vec![2.0f32; DEFAULT_EMBED_DIM])],
        })
        .unwrap();
    let refreshed = store
        .note_embedding_for_path("n.md")
        .unwrap()
        .expect("refreshed by upsert");
    assert!((refreshed[0] - 2.0).abs() < 1e-4);
}

#[test]
fn note_embedding_none_for_empty_note() {
    let (_dir, mut store) = fresh_store();
    let id = new_id();
    store
        .upsert_note(&NoteUpsert {
            id: &id,
            path: "empty.md",
            content_hash: "h",
            mtime: 0,
            size: 0,
            indexed_at: 0,
            embedder_version: "test",
            chunks: vec![],
        })
        .unwrap();
    assert!(store.note_embedding_for_path("empty.md").unwrap().is_none());
}

#[test]
fn upsert_replaces_chunks() {
    let (_dir, mut store) = fresh_store();
    let id = new_id();
    store
        .upsert_note(&NoteUpsert {
            id: &id,
            path: "a.md",
            content_hash: "v1",
            mtime: 1,
            size: 1,
            indexed_at: 1,
            embedder_version: "test",
            chunks: vec![
                (mk_chunk(0, "old0"), unit_vec(0.0)),
                (mk_chunk(1, "old1"), unit_vec(1.0)),
                (mk_chunk(2, "old2"), unit_vec(2.0)),
            ],
        })
        .unwrap();

    // Re-upsert with fewer, different chunks. Old ones must vanish.
    store
        .upsert_note(&NoteUpsert {
            id: &id,
            path: "a.md",
            content_hash: "v2",
            mtime: 2,
            size: 2,
            indexed_at: 2,
            embedder_version: "test",
            chunks: vec![(mk_chunk(0, "new0"), unit_vec(10.0))],
        })
        .unwrap();

    let chunks = store.get_note_chunks(&id).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, "new0");

    // Verify the vec table is also down to one row for this note.
    let conn = store.open_reader().unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunk_vecs WHERE chunk_id LIKE ?1 || ':%'",
            params![id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn delete_note_cascades() {
    let (_dir, mut store) = fresh_store();
    let id = new_id();
    store
        .upsert_note(&NoteUpsert {
            id: &id,
            path: "x.md",
            content_hash: "h",
            mtime: 1,
            size: 1,
            indexed_at: 1,
            embedder_version: "t",
            chunks: vec![(mk_chunk(0, "t"), unit_vec(0.5))],
        })
        .unwrap();

    store.delete_note(&id).unwrap();
    assert!(store.get_note_by_path("x.md").unwrap().is_none());
    assert!(store.get_note_chunks(&id).unwrap().is_empty());
    assert!(!store.note_exists("x.md").unwrap());

    let conn = store.open_reader().unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunk_vecs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn rename_preserves_id_and_chunks() {
    let (_dir, mut store) = fresh_store();
    let id = new_id();
    store
        .upsert_note(&NoteUpsert {
            id: &id,
            path: "old.md",
            content_hash: "h",
            mtime: 1,
            size: 1,
            indexed_at: 1,
            embedder_version: "t",
            chunks: vec![(mk_chunk(0, "body"), unit_vec(0.0))],
        })
        .unwrap();

    store.rename_note(&id, "new.md").unwrap();

    assert!(store.get_note_by_path("old.md").unwrap().is_none());
    let note = store.get_note_by_path("new.md").unwrap().unwrap();
    assert_eq!(note.id, id);

    // Old path no longer maps to the id.
    assert!(!store.note_exists("old.md").unwrap());
    assert_eq!(
        store.get_note_by_path("new.md").unwrap().map(|r| r.id),
        Some(id.clone()),
    );

    // Chunks survived.
    assert_eq!(store.get_note_chunks(&id).unwrap().len(), 1);
}

#[test]
fn knn_finds_nearest_and_excludes_self() {
    let (_dir, mut store) = fresh_store();
    let id_a = new_id();
    let id_b = new_id();
    let id_c = new_id();

    // a's chunks are seeded near 0.0; b's near 0.0 too (so b is "close"
    // to a); c is far away at seed 100.0.
    store
        .upsert_note(&NoteUpsert {
            id: &id_a,
            path: "a.md",
            content_hash: "h",
            mtime: 1,
            size: 1,
            indexed_at: 1,
            embedder_version: "t",
            chunks: vec![(mk_chunk(0, "a-chunk"), unit_vec(0.0))],
        })
        .unwrap();
    store
        .upsert_note(&NoteUpsert {
            id: &id_b,
            path: "b.md",
            content_hash: "h",
            mtime: 1,
            size: 1,
            indexed_at: 1,
            embedder_version: "t",
            chunks: vec![(mk_chunk(0, "b-chunk"), unit_vec(0.001))],
        })
        .unwrap();
    store
        .upsert_note(&NoteUpsert {
            id: &id_c,
            path: "c.md",
            content_hash: "h",
            mtime: 1,
            size: 1,
            indexed_at: 1,
            embedder_version: "t",
            chunks: vec![(mk_chunk(0, "c-chunk"), unit_vec(100.0))],
        })
        .unwrap();

    // Query near 0.0; expect a's chunk first, b's second, c's last.
    let hits = store.knn_chunks(&unit_vec(0.0), 3, None).unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].note_id, id_a);
    assert_eq!(hits[1].note_id, id_b);
    assert_eq!(hits[2].note_id, id_c);
    // Scores monotonically decrease as distance grows.
    assert!(hits[0].score >= hits[1].score);
    assert!(hits[1].score > hits[2].score);

    // Excluding a should drop a's chunks; b ranks first among the rest.
    let hits = store.knn_chunks(&unit_vec(0.0), 3, Some(&id_a)).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].note_id, id_b);
    assert_eq!(hits[1].note_id, id_c);
}

#[test]
fn related_notes_aggregates_by_note() {
    let (_dir, mut store) = fresh_store();
    let id_src = new_id();
    let id_near = new_id();
    let id_far = new_id();

    // Source has two chunks at seeds 0 and 1.
    store
        .upsert_note(&NoteUpsert {
            id: &id_src,
            path: "src.md",
            content_hash: "h",
            mtime: 1,
            size: 1,
            indexed_at: 1,
            embedder_version: "t",
            chunks: vec![
                (mk_chunk(0, "src-c0"), unit_vec(0.0)),
                (mk_chunk(1, "src-c1"), unit_vec(1.0)),
            ],
        })
        .unwrap();

    // "near" has one chunk close to source seed 0; one far away. The
    // aggregation should pick the closer one as its representative.
    store
        .upsert_note(&NoteUpsert {
            id: &id_near,
            path: "near.md",
            content_hash: "h",
            mtime: 1,
            size: 1,
            indexed_at: 1,
            embedder_version: "t",
            chunks: vec![
                (mk_chunk(0, "near-good"), unit_vec(0.001)),
                (mk_chunk(1, "near-bad"), unit_vec(50.0)),
            ],
        })
        .unwrap();

    store
        .upsert_note(&NoteUpsert {
            id: &id_far,
            path: "far.md",
            content_hash: "h",
            mtime: 1,
            size: 1,
            indexed_at: 1,
            embedder_version: "t",
            chunks: vec![(mk_chunk(0, "far-only"), unit_vec(99.0))],
        })
        .unwrap();

    let hits = store.related_notes(&id_src, 5).unwrap();
    // Source note must not be present.
    assert!(!hits.iter().any(|h| h.note_id == id_src));
    // Near should outrank far.
    let near_pos = hits.iter().position(|h| h.note_id == id_near).unwrap();
    let far_pos = hits.iter().position(|h| h.note_id == id_far).unwrap();
    assert!(near_pos < far_pos);
    // The representative chunk for near should be the close one.
    let near_hit = &hits[near_pos];
    assert_eq!(near_hit.title, "near");
    assert!(near_hit.snippet.contains("near-good"));
}

#[test]
fn related_notes_empty_when_source_unindexed() {
    let (_dir, store) = fresh_store();
    let hits = store.related_notes("nonexistent-id", 5).unwrap();
    assert!(hits.is_empty());
}

#[test]
fn embed_dim_mismatch_rejected() {
    let (_dir, mut store) = fresh_store();
    let id = new_id();
    let bad = vec![0.0_f32; DEFAULT_EMBED_DIM - 1];
    let res = store.upsert_note(&NoteUpsert {
        id: &id,
        path: "x.md",
        content_hash: "h",
        mtime: 1,
        size: 1,
        indexed_at: 1,
        embedder_version: "t",
        chunks: vec![(mk_chunk(0, "t"), bad)],
    });
    assert!(matches!(res, Err(Error::EmbedDim { .. })));
}

#[test]
fn knn_dim_mismatch_rejected() {
    let (_dir, store) = fresh_store();
    let res = store.knn_chunks(&[0.0; 10], 5, None);
    assert!(matches!(res, Err(Error::EmbedDim { .. })));
}

#[test]
fn at_autocomplete_orders_by_recency_and_filters_by_basename() {
    let (_dir, mut store) = fresh_store();
    for (path, accessed) in &[
        ("alpha.md", Some(100)),
        ("research/whisper-notes.md", Some(300)),
        ("research/embeddings/whisper-rust.md", Some(200)),
        ("inbox/scratch.md", None),
        ("notes.md", Some(400)),
        ("misc/notes.md", Some(50)),
    ] {
        let id = new_id();
        store
            .upsert_note(&NoteUpsert {
                id: &id,
                path,
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![(mk_chunk(0, "x"), unit_vec(0.0))],
            })
            .unwrap();
        if let Some(ts) = *accessed {
            store.touch_note_access(path, ts).unwrap();
        }
    }

    // Empty prefix → recents-first, NULLs last.
    let recents = store.at_autocomplete("", 10).unwrap();
    let paths: Vec<&str> = recents.iter().map(|s| s.rel_path.as_str()).collect();
    assert_eq!(paths[0], "notes");
    assert_eq!(paths[1], "research/whisper-notes");
    // Extension stripped.
    assert!(!recents.iter().any(|s| s.rel_path.ends_with(".md")));
    // basename + parent_dir populated.
    let whisper = recents
        .iter()
        .find(|s| s.basename == "whisper-notes")
        .unwrap();
    assert_eq!(whisper.parent_dir, "research");

    // Non-empty prefix: substring match against basename, prefix-match
    // ranks ahead of substring-elsewhere.
    let hits = store.at_autocomplete("whisper", 10).unwrap();
    let paths: Vec<&str> = hits.iter().map(|s| s.rel_path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["research/whisper-notes", "research/embeddings/whisper-rust"]
    );

    // Disambiguation: "notes" matches both notes.md and misc/notes.md
    // (both have basename `notes`); each carries its full rel_path.
    let hits = store.at_autocomplete("notes", 10).unwrap();
    let paths: std::collections::HashSet<&str> =
        hits.iter().map(|s| s.rel_path.as_str()).collect();
    assert!(paths.contains("notes"));
    assert!(paths.contains("misc/notes"));
    assert!(paths.contains("research/whisper-notes"));

    // Limit honored.
    let limited = store.at_autocomplete("", 2).unwrap();
    assert_eq!(limited.len(), 2);
}

// status: trail-waypoints-derived-table
#[test]
fn trail_waypoints_insert_query_delete() {
    let (_dir, mut store) = fresh_store();
    let trail_id = "01HTRAIL";
    let wp1 = WaypointRow {
        waypoint_path: ".hiker/trails/01HTRAIL/waypoints/a--AAAAAA.md".into(),
        waypoint_id: "01HWP1".into(),
        trail_id: trail_id.into(),
        source_id: Some("01HSRCA".into()),
        source_path: "research/a.md".into(),
        parent_waypoint_id: None,
        tree_path: "1".into(),
    };
    let wp2 = WaypointRow {
        waypoint_path: ".hiker/trails/01HTRAIL/waypoints/b--BBBBBB.md".into(),
        waypoint_id: "01HWP2".into(),
        trail_id: trail_id.into(),
        source_id: None,
        source_path: "research/b.md".into(),
        parent_waypoint_id: Some("01HWP1".into()),
        tree_path: "1.1".into(),
    };
    store.upsert_trail_waypoint(&wp1).unwrap();
    store.upsert_trail_waypoint(&wp2).unwrap();

    let by_trail = store.waypoints_of(trail_id).unwrap();
    assert_eq!(by_trail.len(), 2);
    assert_eq!(by_trail[0].tree_path, "1");
    assert_eq!(by_trail[1].waypoint_id, "01HWP2");
    assert_eq!(by_trail[1].parent_waypoint_id.as_deref(), Some("01HWP1"));

    // Lookup by source id matches wp1.
    let hits_id = store.trails_containing_note("01HSRCA").unwrap();
    assert_eq!(hits_id.len(), 1);
    assert_eq!(hits_id[0].waypoint_id, "01HWP1");

    // Lookup by source path matches wp2 (no source_id stamped yet).
    let hits_path = store.trails_containing_note("research/b.md").unwrap();
    assert_eq!(hits_path.len(), 1);
    assert_eq!(hits_path[0].waypoint_id, "01HWP2");

    // Re-upsert with mutated source_id is an update, not a new row.
    let wp1_v2 = WaypointRow {
        source_id: Some("01HSRCA-V2".into()),
        ..wp1.clone()
    };
    store.upsert_trail_waypoint(&wp1_v2).unwrap();
    assert_eq!(store.waypoints_of(trail_id).unwrap().len(), 2);
    let hits_v2 = store.trails_containing_note("01HSRCA-V2").unwrap();
    assert_eq!(hits_v2.len(), 1);

    // Single-path delete.
    let removed = store
        .delete_trail_waypoint_by_path(&wp2.waypoint_path)
        .unwrap();
    assert_eq!(removed, 1);
    assert_eq!(store.waypoints_of(trail_id).unwrap().len(), 1);

    // Bulk delete by trail.
    let removed = store.delete_trail_waypoints_by_trail(trail_id).unwrap();
    assert_eq!(removed, 1);
    assert!(store.waypoints_of(trail_id).unwrap().is_empty());
}

// status: trail-waypoints-derived-table
#[test]
fn rename_trail_waypoint_paths_rewrites_prefix() {
    let (_dir, mut store) = fresh_store();
    let row = WaypointRow {
        waypoint_path: ".hiker/trails/01OLD/waypoints/a--AAAAAA.md".into(),
        waypoint_id: "01HWP".into(),
        trail_id: "01OLD".into(),
        source_id: None,
        source_path: "src.md".into(),
        parent_waypoint_id: None,
        tree_path: "1".into(),
    };
    store.upsert_trail_waypoint(&row).unwrap();

    let updated = store
        .rename_trail_waypoint_paths(
            ".hiker/trails/01OLD/",
            ".hiker/trails/01NEW/",
        )
        .unwrap();
    assert_eq!(updated, 1);

    // Reading back via PK requires the new path now.
    let rows = store.waypoints_of("01OLD").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].waypoint_path,
        ".hiker/trails/01NEW/waypoints/a--AAAAAA.md"
    );
}

// status: trail-reference-resolution / store-id-from-oplog
// Path-by-id round-trip retired with the `path_ids` table. The op-log
// now owns the path↔doc_id mapping (`doc-index.db`), so this test moved
// to `core::oplog::tests` (`doc_index_maps_path_to_id`).

#[test]
fn at_autocomplete_skips_skipped_rows() {
    let (_dir, mut store) = fresh_store();
    let id = new_id();
    store
        .upsert_skipped(&id, "huge.md", "file too large", 1, 1)
        .unwrap();
    let hits = store.at_autocomplete("", 10).unwrap();
    assert!(hits.iter().all(|h| h.basename != "huge"));
}

// ---- note metadata index (store-note-metadata-index / store-note-query) ----

/// Create an indexed note at `path` with `mtime` and the given metadata
/// entries; returns its id. Empty chunks — the metadata index doesn't need
/// embeddings.
fn put_note(
    store: &mut Store,
    path: &str,
    mtime: i64,
    meta: &[(&str, &str, Option<f64>)],
) -> String {
    let id = new_id();
    store
        .upsert_note(&NoteUpsert {
            id: &id,
            path,
            content_hash: "h",
            mtime,
            size: 1,
            indexed_at: 0,
            embedder_version: "test",
            chunks: Vec::new(),
        })
        .unwrap();
    let entries: Vec<MetaEntry> = meta
        .iter()
        .map(|(k, v, n)| MetaEntry {
            key: (*k).to_string(),
            value: (*v).to_string(),
            num: *n,
        })
        .collect();
    store.replace_note_metadata(&id, &entries).unwrap();
    id
}

#[test]
fn query_notes_equals_and_tag_membership() {
    let (_dir, mut store) = fresh_store();
    let a = put_note(
        &mut store,
        "projects/a.md",
        100,
        &[("status", "active", None), ("tags", "project", None), ("tags", "rust", None)],
    );
    put_note(
        &mut store,
        "projects/b.md",
        200,
        &[("status", "done", None), ("tags", "project", None)],
    );
    put_note(&mut store, "notes/c.md", 300, &[("tags", "idea", None)]);

    let q = NoteQuery {
        filters: vec![MetaFilter::Equals {
            key: "status".into(),
            value: "active".into(),
        }],
        ..Default::default()
    };
    let res = store.query_notes(&q).unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].note_id, a);

    // Tag membership is just Equals on the list key.
    let q = NoteQuery {
        filters: vec![MetaFilter::Equals {
            key: "tags".into(),
            value: "project".into(),
        }],
        ..Default::default()
    };
    assert_eq!(store.query_notes(&q).unwrap().len(), 2);
}

#[test]
fn query_notes_multi_filter_folder_order_limit() {
    let (_dir, mut store) = fresh_store();
    put_note(
        &mut store,
        "projects/a.md",
        100,
        &[("status", "active", None), ("tags", "project", None)],
    );
    let b = put_note(
        &mut store,
        "projects/b.md",
        300,
        &[("status", "active", None), ("tags", "project", None)],
    );
    // Same metadata but outside the folder — must be excluded.
    put_note(
        &mut store,
        "archive/old.md",
        999,
        &[("status", "active", None), ("tags", "project", None)],
    );

    let q = NoteQuery {
        filters: vec![
            MetaFilter::Equals {
                key: "status".into(),
                value: "active".into(),
            },
            MetaFilter::Equals {
                key: "tags".into(),
                value: "project".into(),
            },
        ],
        folder: Some("projects".into()),
        order: Some(NoteOrder::Mtime { dir: OrderDir::Desc }),
        limit: Some(1),
        select: vec![],
    };
    let res = store.query_notes(&q).unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].note_id, b); // newest within projects/
}

#[test]
fn query_notes_num_range_and_meta_order() {
    let (_dir, mut store) = fresh_store();
    put_note(&mut store, "a.md", 1, &[("priority", "1", Some(1.0))]);
    let b = put_note(&mut store, "b.md", 1, &[("priority", "5", Some(5.0))]);
    let c = put_note(&mut store, "c.md", 1, &[("priority", "3", Some(3.0))]);

    let q = NoteQuery {
        filters: vec![MetaFilter::NumRange {
            key: "priority".into(),
            min: Some(3.0),
            max: None,
        }],
        order: Some(NoteOrder::MetaNum {
            key: "priority".into(),
            dir: OrderDir::Desc,
        }),
        ..Default::default()
    };
    let res = store.query_notes(&q).unwrap();
    let ids: Vec<String> = res.into_iter().map(|r| r.note_id).collect();
    assert_eq!(ids, vec![b, c]); // 5 then 3; priority 1 filtered out
}

#[test]
fn query_notes_select_projects_fields() {
    let (_dir, mut store) = fresh_store();
    put_note(
        &mut store,
        "a.md",
        1,
        &[("status", "active", None), ("tags", "x", None), ("tags", "y", None)],
    );
    let q = NoteQuery {
        filters: vec![MetaFilter::Exists {
            key: "status".into(),
        }],
        select: vec!["status".into(), "tags".into()],
        ..Default::default()
    };
    let res = store.query_notes(&q).unwrap();
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].fields.get("status").map(String::as_str), Some("active"));
    // Multi-valued key joins for display.
    assert_eq!(res[0].fields.get("tags").map(String::as_str), Some("x, y"));
}

#[test]
fn delete_note_clears_metadata() {
    let (_dir, mut store) = fresh_store();
    let a = put_note(&mut store, "a.md", 1, &[("status", "active", None)]);
    store.delete_note(&a).unwrap();
    let q = NoteQuery {
        filters: vec![MetaFilter::Exists {
            key: "status".into(),
        }],
        ..Default::default()
    };
    assert!(store.query_notes(&q).unwrap().is_empty());
}

#[test]
fn replace_note_metadata_overwrites_prior() {
    let (_dir, mut store) = fresh_store();
    let a = put_note(&mut store, "a.md", 1, &[("status", "active", None)]);
    // Re-derive with a different status (simulates an edit + re-ingest).
    store
        .replace_note_metadata(
            &a,
            &[MetaEntry {
                key: "status".into(),
                value: "done".into(),
                num: None,
            }],
        )
        .unwrap();
    let active = NoteQuery {
        filters: vec![MetaFilter::Equals {
            key: "status".into(),
            value: "active".into(),
        }],
        ..Default::default()
    };
    let done = NoteQuery {
        filters: vec![MetaFilter::Equals {
            key: "status".into(),
            value: "done".into(),
        }],
        ..Default::default()
    };
    assert!(store.query_notes(&active).unwrap().is_empty());
    assert_eq!(store.query_notes(&done).unwrap().len(), 1);
}
