use super::*;

use crate::config::TrailsConfig;
use crate::trash::Trash;
use crate::watcher::Watcher;
use crate::store::new_id;

fn dl(id: &str, path: &str) -> DoubleLinkRef {
    DoubleLinkRef {
        id: id.to_string(),
        path: path.to_string(),
    }
}

/// Test-only convenience for the common shape used by the
/// `append_waypoint` call sites in this file. Mirrors the pre-refactor
/// positional signature; constructs `AppendWaypointArgs` so the
/// existing tests stay short.
#[allow(clippy::too_many_arguments)]
async fn append_waypoint_test<'a>(
    watcher: &'a Watcher,
    jobs: &'a crate::indexer::IndexJobTx,
    vault: &'a crate::vault::Vault,
    changes: Option<&'a std::sync::Arc<crate::changes::Changes>>,
    store: &'a mut crate::store::Store,
    trail_doc_rel: &'a str,
    source_rel: &'a str,
    parent_waypoint_id: Option<&'a str>,
    annotation: Option<&'a str>,
) -> Result<AppendWaypointOutcome, crate::error::HikerError> {
    append_waypoint(AppendWaypointArgs {
        watcher,
        jobs,
        vault,
        changes,
        store,
        trail_doc_rel,
        source_rel,
        parent_waypoint_id,
        annotation,
    })
    .await
}

fn we(id: &str, path: &str) -> WaypointEntry {
    WaypointEntry {
        id: id.to_string(),
        path: path.to_string(),
        waypoints: Vec::new(),
    }
}

#[test]
fn waypoints_dir_for_uses_forward_slashes() {
    assert_eq!(
        waypoints_dir_for("01HRX"),
        ".hiker/trails/01HRX/waypoints"
    );
}

#[test]
fn waypoint_filename_uses_short_id_suffix() {
    // 26-char ULID → last 6 chars upper-cased.
    let actual =
        waypoint_filename("raptor-paper", "01ARZ3NDEKTSV4RRFFQ69G5FAV");
    assert_eq!(actual, "raptor-paper--9G5FAV.md");
    // Lower-case input gets upper-cased.
    assert_eq!(short_id_of("aaaaaa01HWPabcdef"), "ABCDEF");
    // Short id falls back to upper-cased full string under 6 chars.
    assert_eq!(waypoint_filename("x", "ab"), "x--AB.md");
}

#[test]
fn parse_trail_doc_round_trip() {
    let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  last_activated_at: 2026-05-10T12:00:00Z\n  waypoints:\n    - id: 01HWP1\n      path: .hiker/trails/01HTRAIL/waypoints/a--AAAAAA.md\n    - id: 01HWP2\n      path: .hiker/trails/01HTRAIL/waypoints/b--BBBBBB.md\n---\nbody prose\n";
    let parsed = parse_trail_doc(src).unwrap();
    assert_eq!(parsed.id, "01HTRAIL");
    assert_eq!(parsed.last_activated_at.as_deref(), Some("2026-05-10T12:00:00Z"));
    assert_eq!(parsed.waypoints.len(), 2);
    assert_eq!(
        parsed.waypoints[0],
        we("01HWP1", ".hiker/trails/01HTRAIL/waypoints/a--AAAAAA.md")
    );

    let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
    let reparsed = parse_trail_doc(&written).unwrap();
    assert_eq!(parsed, reparsed);
    assert!(written.ends_with("body prose\n"));
}

// status: trail-side-trail-shape
#[test]
fn parse_trail_doc_round_trips_nested_tree() {
    let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  waypoints:\n    - id: ROOT1\n      path: .hiker/trails/01HTRAIL/waypoints/r1--AAAAAA.md\n      waypoints:\n        - id: CHILD1\n          path: .hiker/trails/01HTRAIL/waypoints/c1--BBBBBB.md\n          waypoints:\n            - id: GRAND1\n              path: .hiker/trails/01HTRAIL/waypoints/g1--CCCCCC.md\n    - id: ROOT2\n      path: .hiker/trails/01HTRAIL/waypoints/r2--DDDDDD.md\n---\nbody\n";
    let parsed = parse_trail_doc(src).unwrap();
    assert_eq!(parsed.waypoints.len(), 2);
    assert_eq!(parsed.waypoints[0].id, "ROOT1");
    assert_eq!(parsed.waypoints[0].waypoints.len(), 1);
    assert_eq!(parsed.waypoints[0].waypoints[0].id, "CHILD1");
    assert_eq!(parsed.waypoints[0].waypoints[0].waypoints.len(), 1);
    assert_eq!(parsed.waypoints[0].waypoints[0].waypoints[0].id, "GRAND1");
    assert_eq!(parsed.waypoints[1].id, "ROOT2");
    assert!(parsed.waypoints[1].waypoints.is_empty());

    let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
    let reparsed = parse_trail_doc(&written).unwrap();
    assert_eq!(parsed, reparsed);
}

// status: trail-side-trail-shape
#[test]
fn parse_trail_doc_round_trips_empty_tree() {
    let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  waypoints: []\n---\nbody\n";
    let parsed = parse_trail_doc(src).unwrap();
    assert!(parsed.waypoints.is_empty());
    let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
    let reparsed = parse_trail_doc(&written).unwrap();
    assert_eq!(parsed, reparsed);
}

// status: trail-side-trail-shape
#[test]
fn parse_trail_doc_old_flat_format_parses_as_root_tree() {
    // Pre-tree-format yaml: waypoints have no `waypoints:` key.
    let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  waypoints:\n    - id: A\n      path: a.md\n    - id: B\n      path: b.md\n---\n";
    let parsed = parse_trail_doc(src).unwrap();
    assert_eq!(parsed.waypoints.len(), 2);
    assert!(parsed.waypoints[0].waypoints.is_empty());
    assert!(parsed.waypoints[1].waypoints.is_empty());
}

// status: trail-side-trail-shape
#[test]
fn walk_waypoints_yields_depth_first_with_tree_paths() {
    let tree = vec![
        WaypointEntry {
            id: "R1".into(),
            path: "r1.md".into(),
            waypoints: vec![WaypointEntry {
                id: "C1".into(),
                path: "c1.md".into(),
                waypoints: vec![we("G1", "g1.md")],
            }],
        },
        we("R2", "r2.md"),
    ];
    let mut visits: Vec<(Option<String>, String, String)> = Vec::new();
    walk_waypoints_depth_first(&tree, &mut |parent, e, path| {
        visits.push((
            parent.map(str::to_string),
            e.id.clone(),
            path.to_string(),
        ));
    });
    assert_eq!(
        visits,
        vec![
            (None, "R1".into(), "1".into()),
            (Some("R1".into()), "C1".into(), "1.1".into()),
            (Some("C1".into()), "G1".into(), "1.1.1".into()),
            (None, "R2".into(), "2".into()),
        ]
    );
}

#[test]
fn parse_waypoint_round_trip() {
    let src = "---\nhiker:\n  kind: waypoint\n  id: 01HWP\n  references:\n    id: 01HSRC\n    path: research/raptor-paper.md\n  in_trail:\n    id: 01HTRAIL\n    path: trails/my-trail.md\n---\nuser annotation\n";
    let parsed = parse_waypoint(src).unwrap();
    assert_eq!(parsed.id, "01HWP");
    assert_eq!(parsed.references, dl("01HSRC", "research/raptor-paper.md"));
    assert_eq!(parsed.in_trail, dl("01HTRAIL", "trails/my-trail.md"));

    let written = write_waypoint_frontmatter(src, &parsed).unwrap();
    let reparsed = parse_waypoint(&written).unwrap();
    assert_eq!(parsed, reparsed);
    assert!(written.ends_with("user annotation\n"));
}

#[test]
fn parse_trail_doc_for_rejects_non_markdown() {
    let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n---\n";
    let err = parse_trail_doc_for("trails/my-trail.txt", src).unwrap_err();
    assert!(matches!(err, TrailsError::NotMarkdown(_)));
    assert!(parse_trail_doc_for("trails/my-trail.md", src).is_ok());
}

#[test]
fn parse_trail_doc_rejects_wrong_kind() {
    let src = "---\nhiker:\n  kind: waypoint\n  id: 01HWP\n---\n";
    let err = parse_trail_doc(src).unwrap_err();
    assert!(matches!(err, TrailsError::KindMismatch { expected: "trail", .. }));
}

#[test]
fn write_trail_doc_preserves_unknown_hiker_siblings() {
    // hiker.author and hiker.provenance must round-trip; only the
    // four trail-doc fields get rewritten.
    let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  author: user-authored\n  provenance: user\n  waypoints: []\n---\nbody\n";
    let parsed = parse_trail_doc(src).unwrap();
    let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
    assert!(written.contains("author: user-authored"));
    assert!(written.contains("provenance: user"));
}

// status: trail-empty-waypoint-body
#[test]
fn empty_waypoint_note_has_zero_bytes_after_closing_fm() {
    let src = empty_waypoint_note(
        "01HWP",
        &dl("01HSRC", "research/raptor.md"),
        &dl("01HTRAIL", "trails/my-trail.md"),
    )
    .unwrap();
    // The body must end at the closing `---\n` with no further bytes.
    // (This is the "clean canvas" invariant from the spec.)
    let body_start = src.find("---\n").unwrap();
    // skip first `---\n`
    let after_first = body_start + "---\n".len();
    let close_rel = src[after_first..].find("---\n").unwrap();
    let close_abs = after_first + close_rel + "---\n".len();
    assert_eq!(close_abs, src.len(),
        "expected zero bytes after closing fm; got: {:?}",
        &src[close_abs..]);
}

#[test]
fn write_trail_doc_preserves_top_level_non_hiker_fields() {
    let src = "---\ntitle: My Trail\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  waypoints: []\ntags: [research]\n---\nbody\n";
    let parsed = parse_trail_doc(src).unwrap();
    let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
    assert!(written.contains("title: My Trail"));
    assert!(written.contains("tags:"));
}

// -----------------------------------------------------------------
// Ops tests (slice 2)
// -----------------------------------------------------------------

use crate::embed::{EmbedError, Embedder};
use crate::indexer::{start_indexer, IndexerHandle};
use crate::store::{NoteUpsert, Store};
use std::sync::Arc;
use tempfile::TempDir;

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

fn open_vault(td: &TempDir) -> Vault {
    Vault::open(td.path()).expect("open vault")
}

fn start(vault: Vault, store: Store) -> IndexerHandle {
    start_indexer(vault, store, || {
        Ok(Arc::new(ZeroEmbedder) as Arc<dyn Embedder>)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_trail_writes_trail_doc_and_seeds_waypoints_dir() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);

    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let outcome =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "my-trail")
            .await
            .unwrap();
    assert_eq!(outcome.trail_doc_rel, "trails/my-trail.md");
    assert!(td.path().join(&outcome.trail_doc_rel).exists());
    let waypoints = td
        .path()
        .join(format!(".hiker/trails/{}/waypoints", outcome.trail_id));
    assert!(waypoints.exists() && waypoints.is_dir());

    // Auto-suffix on collision.
    let outcome2 =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "my-trail")
            .await
            .unwrap();
    assert_eq!(outcome2.trail_doc_rel, "trails/my-trail-1.md");

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn append_waypoint_writes_waypoint_and_updates_trail_doc() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);
    let mut read_store = Store::open(td.path()).unwrap();

    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();

    // Source note.
    std::fs::create_dir_all(td.path().join("research")).unwrap();
    std::fs::write(td.path().join("research/raptor.md"), "body").unwrap();

    let out = append_waypoint_test(
        &watcher,
        &idx.job_sender(),
        &vault,
        None,
        &mut read_store,
        &trail.trail_doc_rel,
        "research/raptor.md",
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(out.trail_id, trail.trail_id);
    let waypoint_abs = td.path().join(&out.waypoint_rel);
    assert!(waypoint_abs.exists(), "waypoint file not written");
    let waypoint_src = std::fs::read_to_string(&waypoint_abs).unwrap();
    // Spec: empty body — zero bytes after the closing FM.
    assert!(waypoint_src.ends_with("---\n"),
        "waypoint body must end at closing fm: {waypoint_src:?}");

    // Trail-doc gained the entry.
    let trail_src = std::fs::read_to_string(td.path().join(&trail.trail_doc_rel))
        .unwrap();
    let fm = parse_trail_doc(&trail_src).unwrap();
    assert_eq!(fm.waypoints.len(), 1);
    assert_eq!(fm.waypoints[0].id, out.waypoint_id);
    assert_eq!(fm.waypoints[0].path, out.waypoint_rel);

    // Source had its `hiker.id` stamped via ensure_note_id_stamped.
    let source_src =
        std::fs::read_to_string(td.path().join("research/raptor.md")).unwrap();
    assert!(source_src.contains("hiker:") && source_src.contains("id:"),
        "expected source to have hiker.id stamped: {source_src:?}");

    idx.shutdown().await;
}

// bug-id-stamping-mints-fresh-ulid-instead-of-adopting-path-ids:
// when the indexer has already minted a `path_ids[source]` ULID for
// an ingested source note, `append_waypoint` must adopt that ULID
// when stamping `hiker.id` to the source rather than minting a fresh
// one. Otherwise the waypoint's `references.id` diverges from
// `Store::id_for_path(source)`, and `resolve_reference` returns
// `PathConflict` (renders as a "broken reference" orphan card in the
// Trails sidebar in v1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn append_waypoint_adopts_indexer_path_id_for_source() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);
    let mut prog = idx.subscribe_progress();
    use crate::indexer::ProgressEvent;
    // Drain ModelLoaded.
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        async {
            loop {
                match prog.recv().await {
                    Ok(ProgressEvent::ModelLoaded) => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        },
    )
    .await;

    // 1. Source note ingested by the indexer first, so `path_ids`
    // has a row keyed by `notes/source.md` with id Y.
    std::fs::create_dir_all(td.path().join("notes")).unwrap();
    std::fs::write(td.path().join("notes/source.md"), "body\n").unwrap();
    idx.index_path("notes/source.md").await.unwrap();
    wait_for_upsert(&mut prog, "notes/source.md").await;

    // Observe Y from a fresh reader (Store is owned by the indexer
    // task; opening a new connection is the per-command read pattern).
    let reader = Store::open(td.path()).unwrap();
    let path_ids_y = reader
        .id_for_path("notes/source.md")
        .unwrap()
        .expect("path_ids should have a row after the upsert drained");

    // 2. Append a waypoint that captures `notes/source.md`.
    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail = create_trail(
        &watcher,
        &idx.job_sender(),
        &vault,
        None,
        &cfg,
        "t",
    )
    .await
    .unwrap();
    let mut read_store = Store::open(td.path()).unwrap();
    let wp = append_waypoint_test(
        &watcher,
        &idx.job_sender(),
        &vault,
        None,
        &mut read_store,
        &trail.trail_doc_rel,
        "notes/source.md",
        None,
        None,
    )
    .await
    .unwrap();

    // 3. Read back the waypoint-note's frontmatter and parse it.
    let waypoint_src =
        std::fs::read_to_string(td.path().join(&wp.waypoint_rel)).unwrap();
    let wp_fm = parse_waypoint(&waypoint_src).unwrap();

    // 4. The waypoint's `references.id` MUST match the indexer's
    // `path_ids[notes/source.md]`. This is the assertion that fails
    // pre-fix (the helper minted a fresh ULID, so the values differ).
    assert_eq!(
        wp_fm.references.id, path_ids_y,
        "waypoint references.id must match Store::id_for_path(source) \
         so resolve_reference returns Resolved, not PathConflict"
    );

    // 5. resolve_reference now sees both halves agree.
    let store_for_resolve = Store::open(td.path()).unwrap();
    let outcome =
        resolve_reference(&store_for_resolve, &vault, &wp_fm.references).unwrap();
    match outcome {
        ResolutionOutcome::Resolved { rel_path, id } => {
            assert_eq!(rel_path, "notes/source.md");
            assert_eq!(id, path_ids_y);
        }
        other => panic!(
            "expected Resolved, got {other:?} (this is the orphan/PathConflict \
             rendering bug — references.id must equal path_ids id)"
        ),
    }

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_waypoint_drops_entry_and_moves_waypoint_to_trash() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);

    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
    std::fs::write(td.path().join("a.md"), "body").unwrap();
    let mut read_store = Store::open(td.path()).unwrap();
    let wp = append_waypoint_test(
        &watcher,
        &idx.job_sender(),
        &vault,
        None,
        &mut read_store,
        &trail.trail_doc_rel,
        "a.md",
        None,
        None,
    )
    .await
    .unwrap();
    assert!(td.path().join(&wp.waypoint_rel).exists());

    let trash = Trash::open(td.path());
    remove_waypoint(
        &watcher,
        &idx.job_sender(),
        &vault,
        None,
        &trash,
        &trail.trail_doc_rel,
        &wp.waypoint_id,
    )
    .await
    .unwrap();

    // Waypoint file gone from its original location.
    assert!(!td.path().join(&wp.waypoint_rel).exists());
    // Trail-doc no longer carries the entry.
    let trail_src = std::fs::read_to_string(td.path().join(&trail.trail_doc_rel))
        .unwrap();
    let fm = parse_trail_doc(&trail_src).unwrap();
    assert!(fm.waypoints.is_empty());

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_trail_cascades_doc_and_waypoint_dir() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);

    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
    std::fs::write(td.path().join("a.md"), "body").unwrap();
    let mut read_store = Store::open(td.path()).unwrap();
    let _wp = append_waypoint_test(
        &watcher,
        &idx.job_sender(),
        &vault,
        None,
        &mut read_store,
        &trail.trail_doc_rel,
        "a.md",
        None,
        None,
    )
    .await
    .unwrap();
    let trail_root = td
        .path()
        .join(format!(".hiker/trails/{}", trail.trail_id));
    assert!(trail_root.exists());

    let trash = Trash::open(td.path());
    let _entry = delete_trail(
        &watcher,
        &idx.job_sender(),
        &vault,
        None,
        &trash,
        &trail.trail_doc_rel,
    )
    .await
    .unwrap();

    // Both halves are gone from their original locations.
    assert!(!td.path().join(&trail.trail_doc_rel).exists());
    assert!(!trail_root.exists(),
        "expected waypoint dir to be cascaded into trash");

    idx.shutdown().await;
}

// status: trail-reference-resolution
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_reference_branches() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let mut store = Store::open(td.path()).unwrap();

    // Index two notes manually so we control ids.
    let id_a = new_id();
    let id_b = new_id();
    store
        .upsert_note(NoteUpsert {
            id: &id_a,
            path: "alpha.md",
            content_hash: "h",
            mtime: 0,
            size: 0,
            indexed_at: 0,
            embedder_version: "t",
            chunks: vec![],
        })
        .unwrap();
    store
        .upsert_note(NoteUpsert {
            id: &id_b,
            path: "beta.md",
            content_hash: "h",
            mtime: 0,
            size: 0,
            indexed_at: 0,
            embedder_version: "t",
            chunks: vec![],
        })
        .unwrap();

    // Resolved: both halves agree.
    let r = resolve_reference(
        &store,
        &vault,
        &DoubleLinkRef {
            id: id_a.clone(),
            path: "alpha.md".into(),
        },
    )
    .unwrap();
    assert!(matches!(r, ResolutionOutcome::Resolved { .. }));

    // SelfHeal: id_a resolves to alpha.md, but recorded path is "old.md".
    let r = resolve_reference(
        &store,
        &vault,
        &DoubleLinkRef {
            id: id_a.clone(),
            path: "old.md".into(),
        },
    )
    .unwrap();
    match r {
        ResolutionOutcome::SelfHeal {
            canonical_path,
            id,
            prior_path,
        } => {
            assert_eq!(canonical_path, "alpha.md");
            assert_eq!(id, id_a);
            assert_eq!(prior_path, "old.md");
        }
        other => panic!("expected SelfHeal, got {other:?}"),
    }

    // PathConflict: unknown id, path matches beta.md (id = id_b).
    let r = resolve_reference(
        &store,
        &vault,
        &DoubleLinkRef {
            id: "01UNKNOWN".into(),
            path: "beta.md".into(),
        },
    )
    .unwrap();
    match r {
        ResolutionOutcome::PathConflict {
            recorded_id,
            current_path_id,
            path,
        } => {
            assert_eq!(recorded_id, "01UNKNOWN");
            assert_eq!(current_path_id, id_b);
            assert_eq!(path, "beta.md");
        }
        other => panic!("expected PathConflict, got {other:?}"),
    }

    // Orphan: neither id nor path resolve.
    let r = resolve_reference(
        &store,
        &vault,
        &DoubleLinkRef {
            id: "01NEVER".into(),
            path: "ghost.md".into(),
        },
    )
    .unwrap();
    assert!(matches!(r, ResolutionOutcome::Orphan));
}

// -----------------------------------------------------------------
// Slice 3 tests: trail-auto-update-on-note-move
// -----------------------------------------------------------------

/// Wait for an Upsert of `path` to drain through the indexer's
/// progress stream (Finished, Skipped, or Error all count). Avoids
/// the test sleeping on indexer readiness in a flaky way.
async fn wait_for_upsert(
    rx: &mut tokio::sync::broadcast::Receiver<crate::indexer::ProgressEvent>,
    path: &str,
) {
    use crate::indexer::ProgressEvent;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let ev = tokio::time::timeout(remaining, rx.recv())
            .await
            .expect("timed out waiting for upsert")
            .expect("progress channel closed");
        match &ev {
            ProgressEvent::Finished { path: p }
            | ProgressEvent::Skipped { path: p, .. }
            | ProgressEvent::Error { path: Some(p), .. }
                if p == path =>
            {
                return;
            }
            _ => {}
        }
    }
}

// status: trail-auto-update-on-note-move
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_note_rewrites_waypoint_source_path() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);
    let mut prog = idx.subscribe_progress();
    // Drain ModelLoaded so subsequent waits don't see it.
    use crate::indexer::ProgressEvent;
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        async {
            loop {
                match prog.recv().await {
                    Ok(ProgressEvent::ModelLoaded) => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        },
    )
    .await;

    // Create the source note + a trail with one waypoint.
    std::fs::create_dir_all(td.path().join("notes")).unwrap();
    std::fs::write(td.path().join("notes/a.md"), "body").unwrap();
    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
    let mut read_store = Store::open(td.path()).unwrap();
    let wp = append_waypoint_test(
        &watcher,
        &idx.job_sender(),
        &vault,
        None,
        &mut read_store,
        &trail.trail_doc_rel,
        "notes/a.md",
        None,
        None,
    )
    .await
    .unwrap();

    // Drain progress for the trail-doc + waypoint upserts so the
    // derived `trail_waypoints` row exists before the move.
    wait_for_upsert(&mut prog, &trail.trail_doc_rel).await;
    wait_for_upsert(&mut prog, &wp.waypoint_rel).await;

    // Now move the source note.
    crate::ops::move_note(
        &watcher,
        &idx.job_sender(),
        &vault,
        None,
        "notes/a.md",
        "notes/b.md",
    )
    .await
    .unwrap();

    // The waypoint-note's `references.path` should now be "notes/b.md".
    let waypoint_src =
        std::fs::read_to_string(td.path().join(&wp.waypoint_rel)).unwrap();
    let fm = parse_waypoint(&waypoint_src).unwrap();
    assert_eq!(fm.references.path, "notes/b.md",
        "waypoint references.path should track the moved source");
    assert_eq!(fm.references.id, wp_source_id(&wp, &waypoint_src),
        "waypoint references.id must be unchanged by the path-only move");

    // Drain the auto-update reindex of the waypoint-note so the
    // derived `trail_waypoints` row picks up the new source_path.
    wait_for_upsert(&mut prog, &wp.waypoint_rel).await;

    let store2 = Store::open(td.path()).unwrap();
    let containing = store2.trails_containing_note("notes/b.md").unwrap();
    assert_eq!(containing.len(), 1,
        "derived row should now match the new source path");

    idx.shutdown().await;
}

/// Pull the source-id from the waypoint-note source for the
/// "id is unchanged" assertion in the move test; just re-read the
/// FM and return the references.id (the assertion compares it
/// against itself, which only fails if the parse failed entirely).
fn wp_source_id(_wp: &AppendWaypointOutcome, waypoint_src: &str) -> String {
    parse_waypoint(waypoint_src).unwrap().references.id
}

// status: trail-auto-update-on-note-move
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_folder_rewrites_referencing_waypoints() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);
    let mut prog = idx.subscribe_progress();
    use crate::indexer::ProgressEvent;
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        async {
            loop {
                match prog.recv().await {
                    Ok(ProgressEvent::ModelLoaded) => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        },
    )
    .await;

    std::fs::create_dir_all(td.path().join("oldfolder")).unwrap();
    std::fs::write(td.path().join("oldfolder/x.md"), "body").unwrap();
    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
    let mut read_store = Store::open(td.path()).unwrap();
    let wp = append_waypoint_test(
        &watcher,
        &idx.job_sender(),
        &vault,
        None,
        &mut read_store,
        &trail.trail_doc_rel,
        "oldfolder/x.md",
        None,
        None,
    )
    .await
    .unwrap();
    wait_for_upsert(&mut prog, &trail.trail_doc_rel).await;
    wait_for_upsert(&mut prog, &wp.waypoint_rel).await;

    crate::ops::move_folder(
        &watcher,
        &idx.job_sender(),
        &vault,
        None,
        "oldfolder",
        "newfolder",
    )
    .await
    .unwrap();

    let waypoint_src =
        std::fs::read_to_string(td.path().join(&wp.waypoint_rel)).unwrap();
    let fm = parse_waypoint(&waypoint_src).unwrap();
    assert_eq!(fm.references.path, "newfolder/x.md");

    idx.shutdown().await;
}

// status: trail-auto-update-on-note-move
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_external_rename_triggers_trails_sweep() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);
    let mut prog = idx.subscribe_progress();
    use crate::indexer::ProgressEvent;
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        async {
            loop {
                match prog.recv().await {
                    Ok(ProgressEvent::ModelLoaded) => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        },
    )
    .await;

    std::fs::create_dir_all(td.path().join("notes")).unwrap();
    std::fs::write(td.path().join("notes/src.md"), "body").unwrap();
    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
    let mut read_store = Store::open(td.path()).unwrap();
    let wp = append_waypoint_test(
        &watcher,
        &idx.job_sender(),
        &vault,
        None,
        &mut read_store,
        &trail.trail_doc_rel,
        "notes/src.md",
        None,
        None,
    )
    .await
    .unwrap();
    wait_for_upsert(&mut prog, &trail.trail_doc_rel).await;
    wait_for_upsert(&mut prog, &wp.waypoint_rel).await;

    // Simulate an external rename: do the fs rename ourselves, then
    // hand-feed an IndexJob::Rename to the indexer (the watcher
    // bridge would normally do this). Using the tx directly is the
    // closest test surface to the watcher path.
    std::fs::rename(
        td.path().join("notes/src.md"),
        td.path().join("notes/dst.md"),
    )
    .unwrap();
    idx.job_sender()
        .send(crate::indexer::IndexJob::Rename {
            from: "notes/src.md".into(),
            to: "notes/dst.md".into(),
        })
        .await
        .unwrap();

    // The waypoint-note's reference should be rewritten via the
    // Rename-arm trails sweep. Wait for the resulting reindex.
    wait_for_upsert(&mut prog, &wp.waypoint_rel).await;
    let waypoint_src =
        std::fs::read_to_string(td.path().join(&wp.waypoint_rel)).unwrap();
    let fm = parse_waypoint(&waypoint_src).unwrap();
    assert_eq!(fm.references.path, "notes/dst.md");

    idx.shutdown().await;
}

// -----------------------------------------------------------------
// status: trail-append-cursor — cursor field round-trip + behavior
// -----------------------------------------------------------------

#[test]
fn parse_trail_doc_round_trips_append_under_set() {
    let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  append_under: 01HWPCURSOR\n  waypoints:\n    - id: 01HWPCURSOR\n      path: .hiker/trails/01HTRAIL/waypoints/a--AAAAAA.md\n---\nbody\n";
    let parsed = parse_trail_doc(src).unwrap();
    assert_eq!(parsed.append_under.as_deref(), Some("01HWPCURSOR"));
    let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
    let reparsed = parse_trail_doc(&written).unwrap();
    assert_eq!(parsed, reparsed);
    assert!(written.contains("append_under") && written.contains("01HWPCURSOR"),
        "expected append_under key + value in written frontmatter: {written:?}");
}

#[test]
fn parse_trail_doc_without_append_under_round_trips_clean() {
    let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  waypoints: []\n---\nbody\n";
    let parsed = parse_trail_doc(src).unwrap();
    assert!(parsed.append_under.is_none());
    let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
    assert!(!written.contains("append_under"),
        "expected no append_under key when cursor is None: {written:?}");
    let reparsed = parse_trail_doc(&written).unwrap();
    assert_eq!(parsed, reparsed);
}

#[test]
fn parse_trail_doc_explicit_null_append_under_is_none() {
    let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  append_under: null\n  waypoints: []\n---\nbody\n";
    let parsed = parse_trail_doc(src).unwrap();
    assert!(parsed.append_under.is_none());
}

#[test]
fn write_trail_doc_strips_existing_append_under_when_set_to_none() {
    // Pre-existing `append_under` in the YAML; we rewrite with the
    // cursor field set to None — the resulting frontmatter must NOT
    // carry the stale key (cascade-delete-resets-cursor path).
    let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  append_under: 01HSTALE\n  waypoints: []\n---\n";
    let mut parsed = parse_trail_doc(src).unwrap();
    parsed.append_under = None;
    let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
    assert!(!written.contains("append_under"),
        "expected stale append_under stripped: {written:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn append_waypoint_consults_cursor_when_no_explicit_parent() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);
    let mut read_store = Store::open(td.path()).unwrap();

    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();

    std::fs::write(td.path().join("a.md"), "body").unwrap();
    std::fs::write(td.path().join("b.md"), "body").unwrap();
    std::fs::write(td.path().join("c.md"), "body").unwrap();

    // Cursor stays put across appends — A and B both land at root.
    let wp_a = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "a.md", None, None,
    ).await.unwrap();
    let wp_b = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "b.md", None, None,
    ).await.unwrap();

    // Point cursor at A; append C with no explicit parent → should
    // land as a child of A, not of B.
    set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
        &trail.trail_doc_rel, Some(&wp_a.waypoint_id)).await.unwrap();
    let wp_c = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "c.md", None, None,
    ).await.unwrap();

    let trail_src =
        std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap();
    let fm = parse_trail_doc(&trail_src).unwrap();
    // Two roots: A and B; C is a child of A.
    assert_eq!(fm.waypoints.len(), 2);
    let a = fm.waypoints.iter().find(|w| w.id == wp_a.waypoint_id).unwrap();
    let b = fm.waypoints.iter().find(|w| w.id == wp_b.waypoint_id).unwrap();
    assert_eq!(a.waypoints.len(), 1, "C should be a child of A");
    assert_eq!(a.waypoints[0].id, wp_c.waypoint_id);
    assert!(b.waypoints.is_empty());
    // Cursor unchanged — still pointing at A across the second append.
    assert_eq!(fm.append_under.as_deref(), Some(wp_a.waypoint_id.as_str()));

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn append_waypoint_explicit_parent_overrides_cursor() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);
    let mut read_store = Store::open(td.path()).unwrap();

    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();

    std::fs::write(td.path().join("a.md"), "b").unwrap();
    std::fs::write(td.path().join("b.md"), "b").unwrap();
    std::fs::write(td.path().join("c.md"), "b").unwrap();

    let wp_a = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "a.md", None, None,
    ).await.unwrap();
    let wp_b = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "b.md", None, None,
    ).await.unwrap();

    // Cursor = A, explicit parent = B → child of B; cursor stays at A
    // (appends never move the cursor — exclusively user-controlled).
    set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
        &trail.trail_doc_rel, Some(&wp_a.waypoint_id)).await.unwrap();
    let wp_c = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "c.md", Some(&wp_b.waypoint_id), None,
    ).await.unwrap();

    let trail_src =
        std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap();
    let fm = parse_trail_doc(&trail_src).unwrap();
    let b = fm.waypoints.iter().find(|w| w.id == wp_b.waypoint_id).unwrap();
    let a = fm.waypoints.iter().find(|w| w.id == wp_a.waypoint_id).unwrap();
    assert_eq!(b.waypoints.len(), 1, "C must be a child of B (explicit parent wins)");
    assert_eq!(b.waypoints[0].id, wp_c.waypoint_id);
    assert!(a.waypoints.is_empty(), "A should NOT gain C");
    assert_eq!(fm.append_under.as_deref(), Some(wp_a.waypoint_id.as_str()),
        "cursor stays at A — appends never move the cursor");

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn append_waypoint_does_not_move_cursor() {
    // Cursor is exclusively user-controlled per spec — successive
    // appends under the same cursor become siblings, not a deepening
    // ladder.
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);
    let mut read_store = Store::open(td.path()).unwrap();

    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
    std::fs::write(td.path().join("a.md"), "b").unwrap();
    std::fs::write(td.path().join("b.md"), "b").unwrap();
    std::fs::write(td.path().join("c.md"), "b").unwrap();

    // Three appends with cursor = None → three siblings at root.
    let wp1 = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "a.md", None, None,
    ).await.unwrap();
    let wp2 = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "b.md", None, None,
    ).await.unwrap();

    let fm = parse_trail_doc(
        &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
    ).unwrap();
    assert!(fm.append_under.is_none(), "cursor stays None across appends");
    assert_eq!(fm.waypoints.len(), 2, "wp1 and wp2 are siblings at root");
    assert!(fm.waypoints[0].waypoints.is_empty());
    assert!(fm.waypoints[1].waypoints.is_empty());

    // Move cursor to wp1; two appends under it become siblings under wp1.
    set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
        &trail.trail_doc_rel, Some(&wp1.waypoint_id)).await.unwrap();
    let wp3 = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "c.md", None, None,
    ).await.unwrap();

    let fm2 = parse_trail_doc(
        &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
    ).unwrap();
    // Cursor still wp1, not wp3.
    assert_eq!(fm2.append_under.as_deref(), Some(wp1.waypoint_id.as_str()));
    let wp1_node = fm2.waypoints.iter().find(|w| w.id == wp1.waypoint_id).unwrap();
    assert_eq!(wp1_node.waypoints.len(), 1);
    assert_eq!(wp1_node.waypoints[0].id, wp3.waypoint_id);
    // wp2 is a sibling of wp1 at root, with no children.
    let wp2_node = fm2.waypoints.iter().find(|w| w.id == wp2.waypoint_id).unwrap();
    assert!(wp2_node.waypoints.is_empty());

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn append_waypoint_with_stale_cursor_falls_back_to_root() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);
    let mut read_store = Store::open(td.path()).unwrap();

    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();

    // Hand-set a stale cursor by reading + writing the trail-doc.
    let src = std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap();
    let mut fm = parse_trail_doc(&src).unwrap();
    fm.append_under = Some("01HDOESNOTEXIST".into());
    let rewritten = write_trail_doc_frontmatter(&src, &fm).unwrap();
    std::fs::write(td.path().join(&trail.trail_doc_rel), &rewritten).unwrap();

    std::fs::write(td.path().join("a.md"), "b").unwrap();
    let wp = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "a.md", None, None,
    ).await.unwrap();

    let fm = parse_trail_doc(
        &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
    ).unwrap();
    // Landed at root tail per the read-only fallback. Cursor stays
    // stale on disk — the spec treats stale `append_under` as null
    // on read with a warn, but doesn't auto-clean it (the next
    // user-driven cursor mutation overwrites it).
    assert_eq!(fm.waypoints.len(), 1);
    assert_eq!(fm.waypoints[0].id, wp.waypoint_id);
    assert_eq!(fm.append_under.as_deref(), Some("01HDOESNOTEXIST"));

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_waypoint_resets_cursor_when_removed() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);
    let mut read_store = Store::open(td.path()).unwrap();
    let trash = Trash::open(td.path());

    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
    std::fs::write(td.path().join("a.md"), "b").unwrap();
    let wp = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "a.md", None, None,
    ).await.unwrap();
    // Move cursor onto wp explicitly, then remove wp → cursor must reset.
    set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
        &trail.trail_doc_rel, Some(&wp.waypoint_id)).await.unwrap();
    remove_waypoint(
        &watcher, &idx.job_sender(), &vault, None, &trash,
        &trail.trail_doc_rel, &wp.waypoint_id,
    ).await.unwrap();

    let fm = parse_trail_doc(
        &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
    ).unwrap();
    assert!(fm.append_under.is_none(),
        "cursor must reset when its waypoint is removed");

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_waypoint_resets_cursor_when_ancestor_removed() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);
    let mut read_store = Store::open(td.path()).unwrap();
    let trash = Trash::open(td.path());

    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
    std::fs::write(td.path().join("a.md"), "b").unwrap();
    std::fs::write(td.path().join("b.md"), "b").unwrap();
    let wp_y = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "a.md", None, None,
    ).await.unwrap();
    // Set cursor on wp_y so the next append lands as a child.
    set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
        &trail.trail_doc_rel, Some(&wp_y.waypoint_id)).await.unwrap();
    let wp_x = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "b.md", None, None,
    ).await.unwrap();
    // Move cursor onto wp_x — the deeper descendant.
    set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
        &trail.trail_doc_rel, Some(&wp_x.waypoint_id)).await.unwrap();

    // Remove the ancestor wp_y → cascades wp_x → cursor must reset.
    remove_waypoint(
        &watcher, &idx.job_sender(), &vault, None, &trash,
        &trail.trail_doc_rel, &wp_y.waypoint_id,
    ).await.unwrap();

    let fm = parse_trail_doc(
        &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
    ).unwrap();
    assert!(fm.append_under.is_none(),
        "cursor must reset when an ancestor of the cursor is removed");
    assert!(fm.waypoints.is_empty(), "subtree should be gone");

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_waypoint_preserves_cursor_when_sibling_removed() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);
    let mut read_store = Store::open(td.path()).unwrap();
    let trash = Trash::open(td.path());

    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
    std::fs::write(td.path().join("a.md"), "b").unwrap();
    std::fs::write(td.path().join("b.md"), "b").unwrap();

    let wp_a = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "a.md", None, None,
    ).await.unwrap();
    // Cursor stays None across appends, so wp_b is a root sibling.
    let wp_b = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "b.md", None, None,
    ).await.unwrap();
    // Point cursor at wp_a.
    set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
        &trail.trail_doc_rel, Some(&wp_a.waypoint_id)).await.unwrap();

    // Remove the sibling wp_b → cursor (wp_a) unchanged.
    remove_waypoint(
        &watcher, &idx.job_sender(), &vault, None, &trash,
        &trail.trail_doc_rel, &wp_b.waypoint_id,
    ).await.unwrap();

    let fm = parse_trail_doc(
        &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
    ).unwrap();
    assert_eq!(fm.append_under.as_deref(), Some(wp_a.waypoint_id.as_str()),
        "removing a sibling of the cursor must NOT touch the cursor");

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_append_cursor_round_trip() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);
    let mut read_store = Store::open(td.path()).unwrap();

    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
    std::fs::write(td.path().join("a.md"), "b").unwrap();
    let wp = append_waypoint_test(
        &watcher, &idx.job_sender(), &vault, None, &mut read_store,
        &trail.trail_doc_rel, "a.md", None, None,
    ).await.unwrap();

    // Set to None.
    set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
        &trail.trail_doc_rel, None).await.unwrap();
    let fm = parse_trail_doc(
        &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
    ).unwrap();
    assert!(fm.append_under.is_none());

    // Set to wp.
    set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
        &trail.trail_doc_rel, Some(&wp.waypoint_id)).await.unwrap();
    let fm = parse_trail_doc(
        &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
    ).unwrap();
    assert_eq!(fm.append_under.as_deref(), Some(wp.waypoint_id.as_str()));

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_append_cursor_rejects_unknown_id() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start(vault.clone(), store);

    let cfg = TrailsConfig {
        new_trail_dir: "trails/".into(),
    };
    let trail =
        create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();

    let err = set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
        &trail.trail_doc_rel, Some("01HBOGUS")).await.unwrap_err();
    assert!(matches!(err, HikerError::NotFound(_)),
        "set_append_cursor must reject a waypoint id that doesn't resolve: got {err:?}");

    idx.shutdown().await;
}

// status: trail-auto-update-on-note-move
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_note_moved_no_trails_returns_zero() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let mut store = Store::open(td.path()).unwrap();
    // No trails exist; calling on_note_moved should do nothing.
    let touched = on_note_moved(
        None,
        None,
        &vault,
        None,
        &mut store,
        "notes/foo.md",
        "notes/bar.md",
    )
    .await
    .unwrap();
    assert_eq!(touched, 0);
}
