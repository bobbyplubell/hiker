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
