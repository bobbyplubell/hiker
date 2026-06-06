use super::file::{create_with_suffix, delete, move_folder, move_note, restore};
use crate::embed::{Error, Embedder};
// One import for the op-log producer-bridge tests below; the bridge wraps
// `OpLog`, so the tests construct one directly and assert through the bridge.
use crate::oplog::OpLog;
use crate::indexer::{self, Handle};
use crate::store::Store;
use crate::trash::Trash;
use crate::vault::Vault;
use crate::watcher::Watcher;
use std::sync::Arc;
use tempfile::TempDir;

/// Stub embedder so the indexer task starts immediately and emits a
/// ModelLoaded event without needing real model files. Returns a
/// 384-dim zero vector for any input.
struct ZeroEmbedder;
impl Embedder for ZeroEmbedder {
    fn embed_batch(&self, batch: &[String]) -> Result<Vec<Vec<f32>>, Error> {
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

fn start_indexer(vault: Vault, store: Store) -> Handle {
    indexer::start(vault, store, || {
        Ok(Arc::new(ZeroEmbedder) as Arc<dyn Embedder>)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_with_suffix_picks_first_free_slot() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);

    let p1 = create_with_suffix(&watcher, &idx.job_sender(), &vault, "", "new-note")
        .await
        .unwrap();
    assert_eq!(p1, "new-note-1.md");
    let p2 = create_with_suffix(&watcher, &idx.job_sender(), &vault, "", "new-note")
        .await
        .unwrap();
    assert_eq!(p2, "new-note-2.md");

    // Custom template — no collision with new-note-* slots.
    let p3 = create_with_suffix(&watcher, &idx.job_sender(), &vault, "", "draft")
        .await
        .unwrap();
    assert_eq!(p3, "draft-1.md");

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_note_renames_existing_file() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);

    std::fs::write(td.path().join("a.md"), "hello").unwrap();
    move_note(&watcher, &idx.job_sender(), "a.md", "b.md")
        .await
        .unwrap();
    assert!(!td.path().join("a.md").exists());
    assert!(td.path().join("b.md").exists());

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_folder_renames_directory_with_members() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);

    std::fs::create_dir(td.path().join("src")).unwrap();
    std::fs::write(td.path().join("src/a.md"), "x").unwrap();
    std::fs::write(td.path().join("src/b.md"), "y").unwrap();

    move_folder(&watcher, &idx.job_sender(), &vault, "src", "dst")
        .await
        .unwrap();
    assert!(!td.path().join("src").exists());
    assert!(td.path().join("dst/a.md").exists());
    assert!(td.path().join("dst/b.md").exists());

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn move_note_suppresses_watcher_events_for_both_paths() {
    use crate::watcher::FileEvent;
    use std::time::{Duration, Instant};
    use tokio::time::timeout;

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);

    // Subscribe before the op so any event the rename produces lands in
    // our channel. Settle briefly so the watcher's bridge thread is up.
    let mut rx = watcher.subscribe();
    std::fs::write(td.path().join("a.md"), b"x").unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    move_note(&watcher, &idx.job_sender(), "a.md", "b.md")
        .await
        .unwrap();

    // Drive a positive control after the op so we have something
    // unambiguous to wait for; once we see it, no `a.md`/`b.md` event
    // ever surfaced past the watcher's debounce + suppression TTL.
    std::fs::write(td.path().join("decoy.md"), b"y").unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_decoy = false;
    while Instant::now() < deadline && !saw_decoy {
        match timeout(Duration::from_millis(300), rx.recv()).await {
            Ok(Ok(ev)) => {
                let path = match &ev {
                    FileEvent::Created { path } | FileEvent::Modified { path } => {
                        path.clone()
                    }
                    FileEvent::Deleted { path } => path.clone(),
                    FileEvent::Renamed { to, .. } => to.clone(),
                    FileEvent::Overflow => continue,
                };
                assert!(
                    path != "a.md" && path != "b.md",
                    "ops::move_note leaked watcher event for suppressed path: {ev:?}",
                );
                if path == "decoy.md" {
                    saw_decoy = true;
                }
            }
            _ => continue,
        }
    }
    assert!(saw_decoy, "expected to see the decoy write surface");

    idx.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_then_restore_round_trips() {
    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let idx = start_indexer(vault.clone(), store);

    std::fs::write(td.path().join("note.md"), "body").unwrap();

    let entry = delete(&watcher, &idx.job_sender(), &vault, "note.md")
        .await
        .unwrap();
    assert!(!td.path().join("note.md").exists());
    assert_eq!(entry.original_path, "note.md");

    let trash = Trash::open(td.path());
    let restored = restore(&watcher, &idx.job_sender(), &trash, &entry.id)
        .await
        .unwrap();
    assert_eq!(restored.original_path, "note.md");
    assert!(td.path().join("note.md").exists());

    idx.shutdown().await;
}

// ── op-log producer bridge (op-log-doc-id-bootstrap / -ops-producer-helpers)

#[test]
fn bootstrap_seeds_notes_and_is_idempotent() {
    use crate::ops::op_writes as bridge;

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "# A\nbody\n").unwrap();
    std::fs::create_dir_all(td.path().join("sub")).unwrap();
    std::fs::write(td.path().join("sub/b.md"), "hello\n").unwrap();

    let log = OpLog::open(td.path()).unwrap();
    let seeded = bridge::bootstrap(&vault, &log).unwrap();
    assert_eq!(seeded, 2, "both notes seeded on first open");

    // Each note maps to a doc whose accepted text equals the on-disk bytes.
    let a_id = log.doc_id_for_path("a.md").unwrap().expect("a.md mapped");
    assert_eq!(log.materialize_accepted(&a_id).unwrap().text, "# A\nbody\n");

    // Second run is a no-op walk — already-mapped notes are skipped.
    let seeded_again = bridge::bootstrap(&vault, &log).unwrap();
    assert_eq!(seeded_again, 0, "idempotent on second open");

    // A note added after the first walk gets seeded on the next run.
    std::fs::write(td.path().join("c.md"), "c\n").unwrap();
    assert_eq!(bridge::bootstrap(&vault, &log).unwrap(), 1);
}

#[test]
fn bootstrap_skips_non_utf8_and_persists_skip_marker() {
    // status: bug-oplog-bootstrap-nonutf8-warn-spam
    //
    // A non-UTF-8 .md must be silently skipped on bootstrap (never block it),
    // a skip marker must be persisted after the first encounter, and subsequent
    // bootstrap runs must skip silently without re-reading the file.
    use crate::ops::op_writes as bridge;

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("valid.md"), "# Good\nbody\n").unwrap();
    // Write raw non-UTF-8 bytes into a .md file (Shift-JIS-like bytes).
    std::fs::write(td.path().join("binary.md"), b"\x82\xa0\x82\xa2\x82\xa4").unwrap();

    let log = OpLog::open(td.path()).unwrap();

    // First bootstrap: valid note is seeded, non-UTF-8 note is skipped.
    let seeded = bridge::bootstrap(&vault, &log).unwrap();
    assert_eq!(seeded, 1, "only the valid note should be seeded");

    // The valid note has a doc-id; the binary note does not.
    assert!(log.doc_id_for_path("valid.md").unwrap().is_some());
    assert!(log.doc_id_for_path("binary.md").unwrap().is_none());

    // The skip marker must have been persisted for the binary note.
    assert!(
        log.is_bootstrap_skipped("binary.md").unwrap(),
        "skip marker must be recorded after first encounter"
    );

    // Second bootstrap: the skip marker causes the binary note to be bypassed
    // without attempting to re-read it — seeded count is still 0.
    let seeded_again = bridge::bootstrap(&vault, &log).unwrap();
    assert_eq!(seeded_again, 0, "second bootstrap must be a no-op");

    // The binary note still has no doc-id after the second run.
    assert!(log.doc_id_for_path("binary.md").unwrap().is_none());
}

#[test]
fn bootstrap_seeds_hidden_trail_waypoints() {
    // A vault arriving with pre-existing waypoint-notes under
    // `.hiker/trails/<id>/waypoints/` (e.g. via sync, or a fresh open
    // against an existing trails store) must give each waypoint an op-log
    // `doc_id` on bootstrap — otherwise trail integrity breaks until
    // something individually ingests each file. The main
    // `walk_indexable_files` pass prunes at `.hiker/`, so this exercises
    // the second-pass walk over the `.hiker/trails/` carve-out.
    // status: op-log-doc-id-bootstrap
    use crate::ops::op_writes as bridge;

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);

    // A regular vault note for sanity.
    std::fs::write(td.path().join("trails-readme.md"), "# Trails\n").unwrap();

    // Two waypoint-notes pre-existing under .hiker/trails/<id>/waypoints/.
    let wp_dir = td.path().join(".hiker/trails/01HFAKETRAILID000000000000/waypoints");
    std::fs::create_dir_all(&wp_dir).unwrap();
    std::fs::write(wp_dir.join("alpha--7K2A9F.md"), "alpha annotation\n").unwrap();
    std::fs::write(wp_dir.join("beta--3Q8M1B.md"), "beta annotation\n").unwrap();

    // A draft trail-doc at .hiker/trails/drafts/<id>.md.
    let drafts = td.path().join(".hiker/trails/drafts");
    std::fs::create_dir_all(&drafts).unwrap();
    std::fs::write(drafts.join("01HFAKEDRAFTID000000000000.md"), "---\nhiker:\n  kind: trail\n  draft: true\n---\n").unwrap();

    let log = OpLog::open(td.path()).unwrap();
    let seeded = bridge::bootstrap(&vault, &log).unwrap();
    // 1 readme + 2 waypoints + 1 draft trail-doc = 4 seeded docs.
    assert_eq!(seeded, 4);

    // Every waypoint-note has a doc_id mapping.
    let wp_a = ".hiker/trails/01HFAKETRAILID000000000000/waypoints/alpha--7K2A9F.md";
    let wp_b = ".hiker/trails/01HFAKETRAILID000000000000/waypoints/beta--3Q8M1B.md";
    let draft = ".hiker/trails/drafts/01HFAKEDRAFTID000000000000.md";
    assert!(log.doc_id_for_path(wp_a).unwrap().is_some(), "alpha waypoint must be mapped");
    assert!(log.doc_id_for_path(wp_b).unwrap().is_some(), "beta waypoint must be mapped");
    assert!(log.doc_id_for_path(draft).unwrap().is_some(), "draft trail-doc must be mapped");

    // Idempotent: a second bootstrap is a no-op walk.
    assert_eq!(bridge::bootstrap(&vault, &log).unwrap(), 0);
}

#[test]
fn user_save_writes_through_oplog_to_disk() {
    use crate::ops::op_writes as bridge;

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "original\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();

    bridge::user_save(&log, &vault, "a.md", "edited body\n").unwrap();

    // Both the materialized accepted state and the on-disk file reflect the
    // edit (the op-log atomic-write path wrote the .md).
    let id = log.doc_id_for_path("a.md").unwrap().unwrap();
    assert_eq!(log.materialize_accepted(&id).unwrap().text, "edited body\n");
    assert_eq!(
        std::fs::read_to_string(td.path().join("a.md")).unwrap(),
        "edited body\n"
    );

    // Saving a never-seeded path seeds it on demand.
    bridge::user_save(&log, &vault, "fresh.md", "new note\n").unwrap();
    let fresh = log.doc_id_for_path("fresh.md").unwrap().unwrap();
    assert_eq!(log.materialize_accepted(&fresh).unwrap().text, "new note\n");
}

#[test]
fn ensure_doc_seeds_new_note_then_is_idempotent() {
    use crate::ops::op_writes as bridge;

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();

    // Mimic the New Note button: create an empty file after bootstrap, with no
    // op-log document registered for it yet.
    vault.create_note("new-note-1.md").unwrap();
    assert!(log.doc_id_for_path("new-note-1.md").unwrap().is_none());

    // Opening the buffer ensures a doc; the path now resolves so the layered
    // save (`commit_working`) has something to commit onto.
    let id = bridge::ensure_doc(&log, &vault, "new-note-1.md").unwrap();
    assert_eq!(log.doc_id_for_path("new-note-1.md").unwrap().as_deref(), Some(id.as_str()));
    assert_eq!(log.materialize_accepted(&id).unwrap().text, "");

    // Idempotent: a second call returns the same id, not a fresh document.
    let again = bridge::ensure_doc(&log, &vault, "new-note-1.md").unwrap();
    assert_eq!(again, id);
}

#[test]
fn stage_agent_edit_then_flip_op_status() {
    use crate::ops::op_writes::{self as bridge, AgentEdit};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "the quick brown fox\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();

    // Stage an anchored agent edit. It must NOT touch accepted/disk yet.
    let outcome = bridge::stage_agent_edits(
        &log,
        &vault,
        "claude-code",
        "mcp-tool-call",
        "a.md",
        &[AgentEdit { old_str: Some("quick".into()), new_str: "slow".into() }],
    )
    .unwrap();
    assert_eq!(outcome.op_ids.len(), 1);
    let id = log.doc_id_for_path("a.md").unwrap().unwrap();
    assert_eq!(
        log.materialize_accepted(&id).unwrap().text,
        "the quick brown fox\n",
        "pending op does not reach accepted before acceptance"
    );

    // Accept via flip_op_status → applies to accepted and writes the .md.
    bridge::flip_op_status(&log, "a.md", &outcome.op_ids, true).unwrap();
    assert_eq!(
        log.materialize_accepted(&id).unwrap().text,
        "the slow brown fox\n"
    );
    assert_eq!(
        std::fs::read_to_string(td.path().join("a.md")).unwrap(),
        "the slow brown fox\n"
    );

    // Reject path: a second staged edit, rejected, leaves accepted untouched.
    let out2 = bridge::stage_agent_edits(
        &log,
        &vault,
        "claude-code",
        "mcp-tool-call",
        "a.md",
        &[AgentEdit { old_str: Some("brown".into()), new_str: "red".into() }],
    )
    .unwrap();
    bridge::flip_op_status(&log, "a.md", &out2.op_ids, false).unwrap();
    assert_eq!(
        log.materialize_accepted(&id).unwrap().text,
        "the slow brown fox\n",
        "rejected op never enters accepted"
    );
    assert!(log.pending_ops(&id).unwrap().is_empty());
}

#[test]
fn hunk_accept_applies_only_overlapping_ops() {
    use crate::ops::op_writes::{self as bridge, AgentEdit};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    // Two well-separated edit sites so each pending op's affected range is
    // distinct in the materialization.
    std::fs::write(
        td.path().join("a.md"),
        "alpha one\nbeta two\ngamma three\n",
    )
    .unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();

    // Stage two edits in one session (whole batch shares a session id).
    let o1 = bridge::stage_agent_edits(
        &log,
        &vault,
        "claude-code",
        "mcp-tool-call",
        "a.md",
        &[AgentEdit { old_str: Some("alpha".into()), new_str: "ALPHA".into() }],
    )
    .unwrap();
    let o2 = bridge::stage_agent_edits(
        &log,
        &vault,
        "claude-code",
        "mcp-tool-call",
        "a.md",
        &[AgentEdit { old_str: Some("gamma".into()), new_str: "GAMMA".into() }],
    )
    .unwrap();

    // The pending-view shows both edits applied.
    let (accepted, pending) =
        bridge::review_materializations(&log, "a.md", Some("claude-code"))
            .unwrap()
            .unwrap();
    assert_eq!(accepted, "alpha one\nbeta two\ngamma three\n");
    assert!(pending.contains("ALPHA") && pending.contains("GAMMA"));

    // Resolve the hunk covering only the first line ("alpha" is at byte 0..5).
    let in_first = bridge::ops_in_hunk(&log, "a.md", Some("claude-code"), 0, 6).unwrap();
    assert_eq!(in_first, o1.op_ids, "only the alpha op overlaps line 1");
    assert!(
        !in_first.iter().any(|id| o2.op_ids.contains(id)),
        "the gamma op must not be resolved for the line-1 hunk"
    );

    // Accept just the first hunk's op → only ALPHA lands; GAMMA stays pending.
    bridge::flip_op_status(&log, "a.md", &in_first, true).unwrap();
    let id = log.doc_id_for_path("a.md").unwrap().unwrap();
    let after = log.materialize_accepted(&id).unwrap().text;
    assert!(after.contains("ALPHA"), "accepted op landed");
    assert!(
        after.contains("gamma"),
        "the non-overlapping op stayed pending (accepted still has lowercase gamma)"
    );
    // One pending op remains (the gamma edit).
    let remaining = log.pending_ops(&id).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].op_id, o2.op_ids[0]);
}

#[test]
fn hunk_resolution_uses_working_coords_when_user_edited_above() {
    use crate::ops::op_writes::{self as bridge, AgentEdit};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "alpha one\nbeta two\ngamma three\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();
    let id = log.doc_id_for_path("a.md").unwrap().unwrap();

    // Agent stages an edit to the gamma line (accepted bytes 19..31).
    let op = bridge::stage_agent_edits(
        &log,
        &vault,
        "claude-code",
        "mcp-tool-call",
        "a.md",
        &[AgentEdit { old_str: Some("gamma".into()), new_str: "GAMMA".into() }],
    )
    .unwrap();

    // User inserts a long line at the top in the working layer, shifting the
    // gamma line down by 31 bytes (to working bytes 50..62). The review overlay
    // is `working + pending`, so the agent op's affected range must now be
    // resolved in *working* coordinates, not the original accepted ones.
    let prefix = "this is a brand new first line\n"; // 31 bytes
    log.apply_working_edit(&id, 0, 0, prefix).unwrap();

    // Querying the gamma hunk at its working-coord range resolves the op.
    let in_working = bridge::ops_in_hunk(&log, "a.md", Some("claude-code"), 50, 62).unwrap();
    assert_eq!(in_working, op.op_ids, "gamma op resolves in working coords");

    // Querying at the stale accepted-coord range no longer resolves it — the
    // affected range moved with the user's edit (this is the regression the
    // overlay's coordinate fix guards).
    let in_accepted = bridge::ops_in_hunk(&log, "a.md", Some("claude-code"), 19, 31).unwrap();
    assert!(in_accepted.is_empty(), "gamma op no longer sits at accepted bytes 19..31");
}

#[test]
fn auto_reject_on_drift_flips_a_drifted_op() {
    use crate::ops::op_writes::{self as bridge, AgentEdit};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "the quick brown fox\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();
    let id = log.doc_id_for_path("a.md").unwrap().unwrap();

    // Stage an anchored edit on "quick".
    let outcome = bridge::stage_agent_edits(
        &log,
        &vault,
        "claude-code",
        "mcp-tool-call",
        "a.md",
        &[AgentEdit { old_str: Some("quick".into()), new_str: "slow".into() }],
    )
    .unwrap();
    assert_eq!(log.pending_ops(&id).unwrap().len(), 1);

    // A user save rewrites the anchored region away → the op drifts.
    bridge::user_save(&log, &vault, "a.md", "the QUICK brown fox\n").unwrap();
    assert!(
        log.is_pending_drifted(&id, &outcome.op_ids[0]).unwrap(),
        "the anchored op should be drifted after the anchor text changed"
    );

    // Flag off → no-op, the drifted op stays pending.
    let none = bridge::auto_reject_drifted(&log, "a.md", false).unwrap();
    assert!(none.is_empty());
    assert_eq!(log.pending_ops(&id).unwrap().len(), 1);

    // Flag on → the drifted op is auto-rejected.
    let rejected = bridge::auto_reject_drifted(&log, "a.md", true).unwrap();
    assert_eq!(rejected, outcome.op_ids);
    assert!(log.pending_ops(&id).unwrap().is_empty());

    // accepted is unchanged by the reject (still the user's save text).
    assert_eq!(
        log.materialize_accepted(&id).unwrap().text,
        "the QUICK brown fox\n"
    );
}

#[test]
fn retention_gc_drops_old_rejected_rows_and_keeps_fresh() {
    use crate::ops::op_writes::{self as bridge, AgentEdit};
    use crate::oplog::meta::{Filter, OpStatus};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "hello world\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();

    // Stage + reject an op so a `rejected` audit row exists with a fresh ts.
    let o = bridge::stage_agent_edits(
        &log,
        &vault,
        "claude-code",
        "mcp-tool-call",
        "a.md",
        &[AgentEdit { old_str: Some("world".into()), new_str: "there".into() }],
    )
    .unwrap();
    bridge::flip_op_status(&log, "a.md", &o.op_ids, false).unwrap();
    let id = log.doc_id_for_path("a.md").unwrap().unwrap();
    let rejected_now = log
        .query_metadata(&Filter {
            doc_id: Some(id.clone()),
            status: Some(OpStatus::Rejected),
            ..Filter::default()
        })
        .unwrap();
    assert_eq!(rejected_now.len(), 1);

    // Retention GC with generous horizons keeps the fresh rows.
    let (acc, rej) = bridge::run_retention_gc(&log, 365, 14).unwrap();
    assert_eq!((acc, rej), (0, 0), "fresh rows are within retention");

    // A `0` horizon is treated as "no GC", not "drop everything".
    let (acc0, rej0) = bridge::run_retention_gc(&log, 0, 0).unwrap();
    assert_eq!((acc0, rej0), (0, 0));
    assert_eq!(
        log.query_metadata(&Filter {
            status: Some(OpStatus::Rejected),
            ..Filter::default()
        })
        .unwrap()
        .len(),
        1,
        "0-day horizon must not wipe the rejected row"
    );
}

#[test]
fn external_edit_applies_as_external_author() {
    use crate::ops::op_writes as bridge;
    use crate::oplog::meta::{Filter, OpStatus};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "first line\nsecond line\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();
    let id = log.doc_id_for_path("a.md").unwrap().unwrap();

    // Simulate an external editor changing the file on disk.
    std::fs::write(td.path().join("a.md"), "first line\nCHANGED line\n").unwrap();

    let applied = bridge::external_edit(&log, &vault, "a.md").unwrap();
    assert!(applied, "a real disk change reconciles");

    // The delta landed in accepted (and so is materialized verbatim).
    assert_eq!(
        log.materialize_accepted(&id).unwrap().text,
        "first line\nCHANGED line\n"
    );

    // A side-table row authored `external` was written.
    let rows = log
        .query_metadata(&Filter {
            doc_id: Some(id.clone()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap();
    assert!(
        rows.iter().any(|r| r.author.as_wire() == "external"),
        "an author=external op metadata row exists"
    );
}

#[test]
fn external_edit_is_noop_on_self_write_echo() {
    use crate::ops::op_writes as bridge;

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "stable content\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();
    let id = log.doc_id_for_path("a.md").unwrap().unwrap();

    // Disk equals materialize(accepted) (the bootstrap seeded it from disk).
    // A watcher event for this path is a self-write echo → no-op.
    let applied = bridge::external_edit(&log, &vault, "a.md").unwrap();
    assert!(!applied, "disk == accepted is a self-write echo, ignored");
    assert_eq!(
        log.materialize_accepted(&id).unwrap().text,
        "stable content\n"
    );
}

// ── startup disk reconcile (op-log-startup-disk-reconcile) ─────────────────
// Acceptance cases 1, 3, 4, 10 from op-log.md §External-edit sync. Offline
// delete/rename (cases 2, 5–9, 11, 12) are later phases and out of scope here.

#[test]
fn reconcile_disk_folds_offline_edit_as_external() {
    // Acceptance case 1: an offline edit to a tracked file is folded in by the
    // startup pass → materialize(accepted) matches disk, content_hash advances,
    // the op is authored `external`.
    use crate::ops::op_writes as bridge;
    use crate::oplog::meta::{Filter, OpStatus};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "first line\nsecond line\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();
    let id = log.doc_id_for_path("a.md").unwrap().unwrap();
    let hash_before = crate::hash_string(&log.materialize_accepted(&id).unwrap().text);

    // Mutate the .md on disk directly, as an external editor (or sync) would
    // while hiker was closed.
    std::fs::write(td.path().join("a.md"), "first line\nOFFLINE edit\n").unwrap();

    let reconciled = bridge::reconcile_disk(&vault, &log, &Trash::open(td.path())).unwrap();
    assert_eq!(reconciled, 1, "exactly one doc drifted and was reconciled");

    // accepted now matches disk verbatim.
    assert_eq!(
        log.materialize_accepted(&id).unwrap().text,
        "first line\nOFFLINE edit\n"
    );

    // content_hash advanced: the disk text's hash is now in the doc's history.
    let hashes = log.doc_history_hashes(&id).unwrap();
    let hash_after = crate::hash_string("first line\nOFFLINE edit\n");
    assert!(
        hashes.contains(&hash_after),
        "the reconciled content hash is recorded"
    );
    assert_ne!(hash_before, hash_after, "content_hash advanced");

    // The reconcile op is authored `external`.
    let rows = log
        .query_metadata(&Filter {
            doc_id: Some(id.clone()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap();
    assert!(
        rows.iter().any(|r| r.author.as_wire() == "external"),
        "an author=external op metadata row exists"
    );
}

#[test]
fn reconcile_disk_is_noop_when_nothing_changed() {
    // Acceptance case 3: no offline change → reconcile mints nothing (no new
    // op / version) and reports a zero count.
    use crate::ops::op_writes as bridge;
    use crate::oplog::meta::{Filter, OpStatus};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "untouched\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();
    let id = log.doc_id_for_path("a.md").unwrap().unwrap();

    let rows_before = log
        .query_metadata(&Filter {
            doc_id: Some(id.clone()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap()
        .len();

    let reconciled = bridge::reconcile_disk(&vault, &log, &Trash::open(td.path())).unwrap();
    assert_eq!(reconciled, 0, "a clean reopen reconciles nothing");

    let rows_after = log
        .query_metadata(&Filter {
            doc_id: Some(id.clone()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap()
        .len();
    assert_eq!(rows_before, rows_after, "no new accepted op was minted");
    assert_eq!(log.materialize_accepted(&id).unwrap().text, "untouched\n");
}

#[test]
fn reconcile_disk_hash_gate_ignores_mtime_touch() {
    // Acceptance case 4: touch the file (rewrite identical bytes, bumping
    // mtime) but keep the content byte-identical → no op minted. The gate is
    // the byte hash, not mtime.
    use crate::ops::op_writes as bridge;
    use crate::oplog::meta::{Filter, OpStatus};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "identical bytes\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();
    let id = log.doc_id_for_path("a.md").unwrap().unwrap();

    let rows_before = log
        .query_metadata(&Filter {
            doc_id: Some(id.clone()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap()
        .len();

    // Rewrite the exact same bytes — a "touch" that moves mtime forward while
    // leaving content identical (mirrors what a sync round or `touch` does).
    std::fs::write(td.path().join("a.md"), "identical bytes\n").unwrap();

    let reconciled = bridge::reconcile_disk(&vault, &log, &Trash::open(td.path())).unwrap();
    assert_eq!(reconciled, 0, "byte-identical file mints no op (hash gate)");

    let rows_after = log
        .query_metadata(&Filter {
            doc_id: Some(id.clone()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap()
        .len();
    assert_eq!(rows_before, rows_after, "no spurious op from an mtime touch");
}

#[test]
fn reconcile_disk_skips_several_unchanged_docs() {
    // Acceptance case 10: several unchanged docs → reconcile mints nothing for
    // them (and only folds the one that actually drifted).
    use crate::ops::op_writes as bridge;

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "alpha\n").unwrap();
    std::fs::write(td.path().join("b.md"), "bravo\n").unwrap();
    std::fs::create_dir_all(td.path().join("sub")).unwrap();
    std::fs::write(td.path().join("sub/c.md"), "charlie\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();

    // First pass: nothing changed since bootstrap seeded from disk.
    assert_eq!(
        bridge::reconcile_disk(&vault, &log, &Trash::open(td.path())).unwrap(),
        0,
        "all docs unchanged → nothing reconciled"
    );

    // Drift exactly one of the three.
    std::fs::write(td.path().join("b.md"), "BRAVO changed\n").unwrap();
    assert_eq!(
        bridge::reconcile_disk(&vault, &log, &Trash::open(td.path())).unwrap(),
        1,
        "only the one drifted doc is reconciled"
    );

    let b_id = log.doc_id_for_path("b.md").unwrap().unwrap();
    assert_eq!(
        log.materialize_accepted(&b_id).unwrap().text,
        "BRAVO changed\n"
    );

    // A second pass after reconcile is again a no-op (idempotent).
    assert_eq!(bridge::reconcile_disk(&vault, &log, &Trash::open(td.path())).unwrap(), 0);
}

#[test]
fn reconcile_disk_offline_delete_trashes_and_retains_history() {
    // Acceptance case 12: remove a tracked .md from disk → reconcile routes it
    // to trash (history retained, keyed by doc_id) and tombstones the doc as
    // `author=external`; restore then recovers content AND history under the
    // same doc_id with the tombstone cleared.
    use crate::ops::op_writes as bridge;
    use crate::oplog::meta::{Filter, OpStatus};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "line one\nline two\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();
    let id = log.doc_id_for_path("a.md").unwrap().unwrap();
    // Advance history so "history preserved" is a meaningful claim.
    std::fs::write(td.path().join("a.md"), "line one\nline two\nline three\n").unwrap();
    assert_eq!(
        bridge::reconcile_disk(&vault, &log, &Trash::open(td.path())).unwrap(),
        1,
        "the edit folds as one external op"
    );
    let history_len_before_delete = log
        .query_metadata(&Filter {
            doc_id: Some(id.clone()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap()
        .len();
    assert!(history_len_before_delete >= 2, "doc has multi-op history");

    // Offline delete: the file vanishes while hiker is closed.
    std::fs::remove_file(td.path().join("a.md")).unwrap();
    let trash = Trash::open(td.path());
    let reconciled = bridge::reconcile_disk(&vault, &log, &trash).unwrap();
    assert_eq!(reconciled, 1, "the gone file is reconciled as a delete");

    // The doc is tombstoned, authored external.
    assert!(
        log.materialize_accepted(&id).unwrap().tombstone,
        "doc is tombstoned"
    );
    let rows = log
        .query_metadata(&Filter {
            doc_id: Some(id.clone()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap();
    assert!(
        rows.iter()
            .any(|r| r.op_kind == "tombstone" && r.author.as_wire() == "external"),
        "an external Tombstone op was recorded"
    );

    // The op-log history is RETAINED keyed by doc_id (not purged): the
    // pre-delete versions plus the tombstone are all still queryable.
    assert!(
        rows.len() > history_len_before_delete,
        "history retained (pre-delete ops + tombstone), not purged"
    );

    // The content is recoverable from trash: a manifest entry references the
    // doc_id and the artifact carries the last known content.
    let listed = trash.list().unwrap();
    assert_eq!(listed.len(), 1, "one trash entry created");
    let entry = &listed[0];
    assert_eq!(entry.original_path, "a.md");
    assert_eq!(entry.doc_id.as_deref(), Some(id.as_str()), "entry references the doc_id");
    let artifact = std::fs::read_to_string(trash.entry_path(entry)).unwrap();
    assert_eq!(
        artifact, "line one\nline two\nline three\n",
        "trash artifact holds the last known content"
    );

    // Restore: fs-move the artifact back, then rebind the doc — exactly what
    // the indexer's handle_restore_from_trash does (fs restore + oplog
    // writes::restore). Content AND history recover under the same doc_id.
    crate::vault::restore_note(&vault, None, &trash, &entry.id).unwrap();
    crate::oplog::writes::restore(
        &log,
        entry.doc_id.as_deref().unwrap(),
        &entry.original_path,
        &crate::oplog::shapes::Author::User,
    )
    .unwrap();

    // Same doc_id resolves at the restored path.
    assert_eq!(
        log.doc_id_for_path("a.md").unwrap().as_deref(),
        Some(id.as_str()),
        "path rebinds to the same retained doc_id"
    );
    // Tombstone cleared, content recovered.
    let restored = log.materialize_accepted(&id).unwrap();
    assert!(!restored.tombstone, "tombstone cleared on restore");
    assert_eq!(restored.text, "line one\nline two\nline three\n");
    // The file is back on disk.
    assert!(td.path().join("a.md").exists());
    // History survives the round trip (pre-delete ops are still there).
    let after_restore = log
        .query_metadata(&Filter {
            doc_id: Some(id.clone()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap();
    assert!(
        after_restore.len() > rows.len(),
        "restore appends a resurrection op on top of the retained history"
    );
}

#[test]
fn reconcile_disk_offline_rename_rebinds_and_preserves_history() {
    // Offline rename: rename the .md on disk (old gone, new present, identical
    // bytes) → reconcile rebinds path → doc_id to the new path, records a
    // Rename { from } op, preserves history, and creates NO trash entry.
    use crate::ops::op_writes as bridge;
    use crate::oplog::meta::{Filter, OpStatus};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("old.md"), "stable content\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();
    let id = log.doc_id_for_path("old.md").unwrap().unwrap();

    // Offline rename while hiker is closed: same bytes at a new path.
    std::fs::rename(td.path().join("old.md"), td.path().join("new.md")).unwrap();

    let trash = Trash::open(td.path());
    let reconciled = bridge::reconcile_disk(&vault, &log, &trash).unwrap();
    assert_eq!(reconciled, 1, "the rename is recognized as one reconcile");

    // The mapping moved to the new path; the same doc_id, not a fresh one.
    assert_eq!(
        log.path_for_doc(&id).unwrap().as_deref(),
        Some("new.md"),
        "path_for_doc now resolves to the new path"
    );
    assert_eq!(
        log.doc_id_for_path("new.md").unwrap().as_deref(),
        Some(id.as_str()),
    );
    assert!(
        log.doc_id_for_path("old.md").unwrap().is_none(),
        "the old path no longer maps to the doc"
    );

    // A Rename { from: old.md } op authored external was recorded.
    let rows = log
        .query_metadata(&Filter {
            doc_id: Some(id.clone()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap();
    assert!(
        rows.iter().any(|r| {
            r.op_kind == "rename"
                && r.rename_from.as_deref() == Some("old.md")
                && r.author.as_wire() == "external"
        }),
        "an external Rename {{ from: old.md }} op exists"
    );

    // Content (history) preserved, not tombstoned.
    let mat = log.materialize_accepted(&id).unwrap();
    assert!(!mat.tombstone, "a rename does not tombstone");
    assert_eq!(mat.text, "stable content\n");

    // No spurious delete/trash entry was created.
    assert!(
        trash.list().unwrap().is_empty(),
        "an offline rename produces no trash entry"
    );
}

#[test]
fn reconcile_disk_does_not_trash_a_present_but_unreadable_file() {
    // Adversarial: a tracked file that becomes non-UTF-8 (corruption, a binary
    // write) is PRESENT but unreadable — it must NOT be mistaken for an offline
    // delete and trashed. And one unreadable doc must not abort the pass: a
    // sibling with a clean offline edit still reconciles (best-effort per doc).
    use crate::ops::op_writes as bridge;

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("bad.md"), "valid utf8\n").unwrap();
    std::fs::write(td.path().join("good.md"), "good v1\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();
    let bad = log.doc_id_for_path("bad.md").unwrap().unwrap();
    let good = log.doc_id_for_path("good.md").unwrap().unwrap();

    // bad.md becomes invalid UTF-8 while still present; good.md gets a clean edit.
    std::fs::write(td.path().join("bad.md"), [0xff, 0xfe, 0x00, 0x9f]).unwrap();
    std::fs::write(td.path().join("good.md"), "good v2\n").unwrap();

    let trash = Trash::open(td.path());
    let reconciled = bridge::reconcile_disk(&vault, &log, &trash).unwrap();

    // Only good.md reconciled; the unreadable file was skipped, not deleted —
    // and skipping it did NOT abort the pass.
    assert_eq!(reconciled, 1, "the unreadable file did not abort the pass");
    assert_eq!(log.materialize_accepted(&good).unwrap().text, "good v2\n");
    assert!(
        !log.materialize_accepted(&bad).unwrap().tombstone,
        "a present-but-unreadable file must not be tombstoned"
    );
    assert_eq!(
        log.materialize_accepted(&bad).unwrap().text,
        "valid utf8\n",
        "the unreadable file's accepted content is left untouched"
    );
    assert!(trash.list().unwrap().is_empty(), "nothing was trashed");
}

#[test]
fn reconcile_disk_resurrects_tombstoned_doc_when_file_reappears() {
    // Adversarial: an offline delete tombstones a doc + trashes its content. If
    // the file later REAPPEARS on disk (a restore from backup, a sync
    // re-create), the next reconcile must un-delete it — clear the tombstone and
    // fold the current bytes — not leave a tombstoned ghost the tree won't show.
    use crate::ops::op_writes as bridge;

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "v1\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();
    let id = log.doc_id_for_path("a.md").unwrap().unwrap();
    let trash = Trash::open(td.path());

    // Offline delete: file gone → reconcile tombstones + trashes.
    std::fs::remove_file(td.path().join("a.md")).unwrap();
    assert_eq!(bridge::reconcile_disk(&vault, &log, &trash).unwrap(), 1);
    assert!(
        log.materialize_accepted(&id).unwrap().tombstone,
        "doc is tombstoned after the offline delete"
    );

    // The file reappears with NEW content → next reconcile resurrects + folds.
    std::fs::write(td.path().join("a.md"), "v2 resurrected\n").unwrap();
    assert_eq!(bridge::reconcile_disk(&vault, &log, &trash).unwrap(), 1);
    let m = log.materialize_accepted(&id).unwrap();
    assert!(!m.tombstone, "the reappeared file un-tombstones the doc");
    assert_eq!(m.text, "v2 resurrected\n", "the reappeared content is folded in");
    // Same doc_id (history preserved), not a fresh lineage.
    assert_eq!(
        log.doc_id_for_path("a.md").unwrap().as_deref(),
        Some(id.as_str()),
        "the resurrected doc keeps its doc_id"
    );
}

#[test]
fn reconcile_before_bootstrap_keeps_offline_rename_one_lineage() {
    // Ordering invariant (op-log.md §External-edit sync): at vault open the
    // order is reconcile → bootstrap-seed → first sync round, and reconcile
    // MUST run before bootstrap. This test models the REAL startup order for an
    // offline rename: seed `old.md`, simulate close, rename old→new on disk,
    // then run `reconcile_disk` FOLLOWED BY `bootstrap` (the real open order).
    //
    // Correct order ⇒ reconcile claims `new.md` for the existing lineage while
    // it is still untracked (rebind + Rename{from}), so the subsequent bootstrap
    // sees `new.md` already mapped and seeds nothing fresh: ONE doc, ONE
    // lineage, NO trash. A regression to bootstrap-first would seed `new.md` as
    // a fresh doc, leaving two lineages and orphaned history — this guards it.
    use crate::ops::op_writes as bridge;
    use crate::oplog::meta::{Filter, OpStatus};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("old.md"), "stable content\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();
    let id = log.doc_id_for_path("old.md").unwrap().unwrap();

    // Simulate close + offline rename (old gone, new present, same bytes).
    std::fs::rename(td.path().join("old.md"), td.path().join("new.md")).unwrap();

    // The REAL open order: reconcile FIRST, then bootstrap.
    let trash = Trash::open(td.path());
    let reconciled = bridge::reconcile_disk(&vault, &log, &trash).unwrap();
    assert_eq!(reconciled, 1, "reconcile recognizes the rename");
    let seeded = bridge::bootstrap(&vault, &log).unwrap();
    assert_eq!(
        seeded, 0,
        "bootstrap seeds nothing fresh for new.md — reconcile already claimed it"
    );

    // Exactly ONE doc exists for the lineage (no second fresh doc was seeded).
    assert_eq!(
        log.list_doc_ids().unwrap().len(),
        1,
        "only one document (the original lineage) exists after the real open order"
    );

    // It is the SAME doc_id, now mapped to new.md; old.md no longer maps.
    assert_eq!(
        log.path_for_doc(&id).unwrap().as_deref(),
        Some("new.md"),
        "the original lineage moved to new.md"
    );
    assert_eq!(
        log.doc_id_for_path("new.md").unwrap().as_deref(),
        Some(id.as_str()),
        "new.md maps to the original doc_id, not a fresh one"
    );
    assert!(
        log.doc_id_for_path("old.md").unwrap().is_none(),
        "old.md no longer maps to any doc"
    );

    // A Rename { from: old.md } op authored external was recorded — history
    // is preserved on the same lineage, not orphaned to a new one.
    let rows = log
        .query_metadata(&Filter {
            doc_id: Some(id.clone()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap();
    assert!(
        rows.iter().any(|r| {
            r.op_kind == "rename"
                && r.rename_from.as_deref() == Some("old.md")
                && r.author.as_wire() == "external"
        }),
        "an external Rename {{ from: old.md }} op exists on the lineage"
    );
    let mat = log.materialize_accepted(&id).unwrap();
    assert!(!mat.tombstone, "the lineage is not tombstoned");
    assert_eq!(mat.text, "stable content\n", "content/history preserved");

    // And NO trash entry was produced (a rename is not a delete).
    assert!(
        trash.list().unwrap().is_empty(),
        "the real open order produces no trash entry for a rename"
    );
}

#[test]
fn open_time_external_edit_folds_watcher_missed_change() {
    // Open-time reconcile (op-log-open-time-disk-reconcile): a change made
    // directly to a tracked .md on disk that the in-session watcher dropped
    // (suppressed-write window / notify overflow) is folded when the doc is
    // reconciled at buffer open via `external_edit`, and the text the buffer
    // would load (materialize(accepted), == disk after the fold) reflects it.
    use crate::ops::op_writes as bridge;

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("note.md"), "original body\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();
    let id = log.doc_id_for_path("note.md").unwrap().unwrap();

    // A watcher-missed change: disk now diverges from accepted.
    std::fs::write(td.path().join("note.md"), "edited out of band\n").unwrap();

    // Opening the buffer reconciles this one doc before its text loads.
    let applied = bridge::external_edit(&log, &vault, "note.md").unwrap();
    assert!(applied, "the watcher-missed change folds at open");

    // The buffer loads from the reconciled accepted, which now equals disk.
    assert_eq!(
        log.materialize_accepted(&id).unwrap().text,
        "edited out of band\n",
        "the loaded text reflects the out-of-band edit"
    );

    // A second open with no further change is a hash-gated no-op.
    let again = bridge::external_edit(&log, &vault, "note.md").unwrap();
    assert!(!again, "a clean reopen reconciles nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_app_delete_then_restore_preserves_doc_id_and_history() {
    // Regression: the in-app delete (IndexJob::DeleteNote) + restore
    // (IndexJob::RestoreFromTrash) round trip preserves the doc_id and
    // history — restore rebinds rather than minting a fresh import.
    use crate::ops::file::{delete, restore};
    use crate::oplog::meta::{Filter, OpStatus};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let watcher = Watcher::start(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    std::fs::write(td.path().join("keep.md"), "body before delete\n").unwrap();

    // Stand up an op-log and seed it, then attach it to the indexer so the
    // delete / restore jobs record tombstone + rebind through it.
    let log = Arc::new(OpLog::open(td.path()).unwrap());
    crate::ops::op_writes::bootstrap(&vault, &log).unwrap();
    let id = log.doc_id_for_path("keep.md").unwrap().unwrap();

    let idx = start_indexer(vault.clone(), store);
    idx.attach_oplog(log.clone());

    let trash = Trash::open(td.path());

    // Delete → trash; the entry should carry the doc_id and the op-log should
    // tombstone the doc.
    let entry = delete(&watcher, &idx.job_sender(), &vault, "keep.md")
        .await
        .unwrap();
    assert_eq!(entry.doc_id.as_deref(), Some(id.as_str()), "trash entry carries doc_id");
    assert!(log.materialize_accepted(&id).unwrap().tombstone, "doc tombstoned");
    let history_after_delete = log
        .query_metadata(&Filter {
            doc_id: Some(id.clone()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap()
        .len();

    // Restore → rebind; same doc_id, tombstone cleared, history preserved.
    let restored = restore(&watcher, &idx.job_sender(), &trash, &entry.id)
        .await
        .unwrap();
    assert_eq!(restored.original_path, "keep.md");
    assert_eq!(
        log.doc_id_for_path("keep.md").unwrap().as_deref(),
        Some(id.as_str()),
        "restore rebinds the same doc_id (no fresh ULID)"
    );
    let restored_doc = log.materialize_accepted(&id).unwrap();
    assert!(!restored_doc.tombstone, "tombstone cleared");
    assert_eq!(restored_doc.text, "body before delete\n");
    let history_after_restore = log
        .query_metadata(&Filter {
            doc_id: Some(id.clone()),
            status: Some(OpStatus::Accepted),
            ..Filter::default()
        })
        .unwrap()
        .len();
    assert!(
        history_after_restore > history_after_delete,
        "history preserved + a resurrection op appended"
    );
    // The trash entry is gone after a successful restore.
    assert!(trash.find(&entry.id).unwrap().is_none());
}

#[test]
fn hunk_reject_leaves_accepted_untouched() {
    use crate::ops::op_writes::{self as bridge, AgentEdit};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "hello world\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();

    let o = bridge::stage_agent_edits(
        &log,
        &vault,
        "claude-code",
        "mcp-tool-call",
        "a.md",
        &[AgentEdit { old_str: Some("world".into()), new_str: "there".into() }],
    )
    .unwrap();

    // The op overlaps a hunk covering the whole line.
    let resolved = bridge::ops_in_hunk(&log, "a.md", Some("claude-code"), 0, 12).unwrap();
    assert_eq!(resolved, o.op_ids);

    // Reject → accepted + disk unchanged, op dropped, rejected audit row written.
    bridge::flip_op_status(&log, "a.md", &resolved, false).unwrap();
    let id = log.doc_id_for_path("a.md").unwrap().unwrap();
    assert_eq!(log.materialize_accepted(&id).unwrap().text, "hello world\n");
    assert_eq!(
        std::fs::read_to_string(td.path().join("a.md")).unwrap(),
        "hello world\n"
    );
    assert!(log.pending_ops(&id).unwrap().is_empty());
}

#[test]
fn list_whole_file_proposals_keeps_only_write_note_shape() {
    use crate::ops::op_writes::{self as bridge, AgentEdit};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "the quick brown fox\n").unwrap();
    std::fs::write(td.path().join("b.md"), "second note\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();

    // A whole-body rewrite (write_note shape): old_str = None.
    let whole = bridge::stage_agent_edits(
        &log,
        &vault,
        "claude-code",
        "mcp-tool-call",
        "a.md",
        &[AgentEdit { old_str: None, new_str: "wholly new body\n".into() }],
    )
    .unwrap();
    // An anchored edit (edit_note shape) on b.md must NOT surface here.
    bridge::stage_agent_edits(
        &log,
        &vault,
        "claude-code",
        "mcp-tool-call",
        "b.md",
        &[AgentEdit { old_str: Some("second".into()), new_str: "first".into() }],
    )
    .unwrap();

    let props = bridge::list_whole_file_proposals(&log).unwrap();
    assert_eq!(props.len(), 1, "only the whole-body rewrite is a proposal");
    let p = &props[0];
    assert_eq!(p.op_id, whole.op_ids[0]);
    assert_eq!(p.target_path, "a.md");
    assert_eq!(p.action, "write_note");
    assert!(!p.drifted);

    // The proposal previews via materialize_pending_view (the proposed body)
    // against materialize_accepted (still the on-disk body).
    let (accepted, pending_view) =
        bridge::review_materializations(&log, "a.md", Some("claude-code"))
            .unwrap()
            .unwrap();
    assert_eq!(accepted, "the quick brown fox\n");
    assert_eq!(pending_view, "wholly new body\n");

    // Accept via flip_op_status drains the proposal from the listing.
    bridge::flip_op_status(&log, "a.md", &whole.op_ids, true).unwrap();
    assert!(bridge::list_whole_file_proposals(&log).unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(td.path().join("a.md")).unwrap(),
        "wholly new body\n"
    );
}

#[test]
fn list_pending_proposals_covers_every_pending_op_kind() {
    use crate::ops::op_writes::{self as bridge, AgentEdit};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    std::fs::write(td.path().join("a.md"), "the quick brown fox\n").unwrap();
    std::fs::write(td.path().join("b.md"), "second note\n").unwrap();
    let log = OpLog::open(td.path()).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();

    // count is zero before any pending op is staged.
    assert_eq!(bridge::pending_op_count(&log).unwrap(), 0);

    // Anchored edit_note on a.md (NOT a whole-file proposal).
    let anchored = bridge::stage_agent_edits(
        &log,
        &vault,
        "claude-code",
        "mcp-tool-call",
        "a.md",
        &[AgentEdit { old_str: Some("quick".into()), new_str: "slow".into() }],
    )
    .unwrap();
    // Whole-body rewrite on b.md.
    let whole = bridge::stage_agent_edits(
        &log,
        &vault,
        "claude-code",
        "mcp-tool-call",
        "b.md",
        &[AgentEdit { old_str: None, new_str: "rewritten\n".into() }],
    )
    .unwrap();

    // Unlike list_whole_file_proposals, the cross-vault listing surfaces BOTH.
    let props = bridge::list_pending_proposals(&log).unwrap();
    assert_eq!(props.len(), 2);
    assert_eq!(bridge::pending_op_count(&log).unwrap(), 2);
    let by_path: std::collections::HashMap<&str, &bridge::PendingProposal> =
        props.iter().map(|p| (p.target_path.as_str(), p)).collect();
    assert_eq!(by_path["a.md"].action, "edit_note");
    assert_eq!(by_path["a.md"].op_id, anchored.op_ids[0]);
    assert!(!by_path["a.md"].drifted);
    assert_eq!(by_path["b.md"].action, "write_note");
    assert_eq!(by_path["b.md"].op_id, whole.op_ids[0]);

    // Accept the anchored op; the listing and count both shrink to the
    // remaining whole-file op.
    bridge::flip_op_status(&log, "a.md", &anchored.op_ids, true).unwrap();
    let props = bridge::list_pending_proposals(&log).unwrap();
    assert_eq!(props.len(), 1);
    assert_eq!(props[0].target_path, "b.md");
    assert_eq!(bridge::pending_op_count(&log).unwrap(), 1);
}

#[test]
fn reextract_replaces_linked_skips_unlinked() {
    use crate::ops::op_writes::{self as bridge, ReextractOutcome};

    let td = TempDir::new().unwrap();
    let vault = open_vault(&td);
    let log = OpLog::open(td.path()).unwrap();

    // A LINKED sidecar (fill_body: true / link_state: linked) — the default for
    // an extracted body.
    let linked = "---\nhiker:\n  fill_body: true\n  link_state: linked\n---\nold extracted\n";
    std::fs::create_dir_all(td.path().join("clips")).unwrap();
    std::fs::write(td.path().join("clips/linked.md"), linked).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();

    // Re-extraction of a LINKED sidecar replaces the body in place.
    let out = bridge::reextract(&log, &vault, "clips/linked.md", "new extracted\n", "web").unwrap();
    assert_eq!(out, ReextractOutcome::Replaced);
    let id = log.doc_id_for_path("clips/linked.md").unwrap().unwrap();
    assert!(log.materialize_accepted(&id).unwrap().text.contains("new extracted"));
    assert_eq!(log.doc_history(&id, 100).unwrap()[0].author,
        crate::oplog::shapes::Author::Extractor("web".to_string()));

    // An identical re-extraction is a no-op (no new version).
    let body_now = {
        let t = log.materialize_accepted(&id).unwrap().text;
        let fe = crate::oplog::shapes::frontmatter_fence_end(&t).unwrap();
        t[fe..].to_string()
    };
    let before = log.doc_history(&id, 100).unwrap().len();
    let again = bridge::reextract(&log, &vault, "clips/linked.md", &body_now, "web").unwrap();
    assert_eq!(again, ReextractOutcome::Unchanged);
    assert_eq!(log.doc_history(&id, 100).unwrap().len(), before);

    // An UNLINKED sidecar: re-extraction must NOT overwrite the user's body.
    let unlinked = "---\nhiker:\n  fill_body: false\n  link_state: unlinked\n---\nhand-edited\n";
    std::fs::write(td.path().join("clips/unlinked.md"), unlinked).unwrap();
    bridge::bootstrap(&vault, &log).unwrap();
    let uid = log.doc_id_for_path("clips/unlinked.md").unwrap().unwrap();
    let out = bridge::reextract(&log, &vault, "clips/unlinked.md", "robot text\n", "web").unwrap();
    assert_eq!(out, ReextractOutcome::Skipped);
    // Body untouched; no extractor op landed.
    assert!(log.materialize_accepted(&uid).unwrap().text.contains("hand-edited"));
    assert!(log.doc_history(&uid, 100).unwrap().iter()
        .all(|m| !matches!(m.author, crate::oplog::shapes::Author::Extractor(_))));
}
