use rusqlite::params;

use super::Staging;
use crate::changes::{ChangeOp, Changes};
use crate::hash_string;
use crate::staging::patch::{apply_edit, EditPayload};
use crate::staging::error::Error;
use crate::staging::types::{
    ConflictReason, EditProposalInput, Filter, ProposalInput, ProposalState, ACTION_MOVE_NOTE,
};
use crate::test_helpers::test_staging as staged;
use crate::vault::Vault;
use tempfile::tempdir;

/// status: staging-action-move-note
#[test]
fn propose_move_note_persists_source_path_and_recheck_flips_on_drift() {
    let (_dir, s) = staged();
    let id = s
        .propose(&ProposalInput {
            surface: "triage".into(),
            action: ACTION_MOVE_NOTE.into(),
            target_path: "research/embeddings/voyage.md".into(),
            trail_id: None,
            content: None,
            metadata: None,
            source_hash: None,
            source_path: Some("inbox/voyage.md".into()),
        })
        .unwrap();

    let list = s.list(&Filter::default()).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, id);
    assert_eq!(list[0].action, ACTION_MOVE_NOTE);
    assert_eq!(list[0].source_path.as_deref(), Some("inbox/voyage.md"));
    assert_eq!(list[0].state, ProposalState::Applyable);

    // source vanished → SourceMissing.
    let r = s.recheck_move(&id, false, false).unwrap();
    assert_eq!(r.new_state, ProposalState::Conflicted);
    assert_eq!(r.new_reason, Some(ConflictReason::SourceMissing));

    // back to both present + target free → applyable again.
    let r = s.recheck_move(&id, true, false).unwrap();
    assert_eq!(r.new_state, ProposalState::Applyable);
    assert_eq!(r.new_reason, None);

    // target occupied → TargetOccupied.
    let r = s.recheck_move(&id, true, true).unwrap();
    assert_eq!(r.new_state, ProposalState::Conflicted);
    assert_eq!(r.new_reason, Some(ConflictReason::TargetOccupied));
}

#[test]
fn propose_returns_id_and_appears_in_list() {
    let (_dir, s) = staged();
    let id = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/test.md".into(),
            trail_id: None,
            content: Some("# Hello".into()),
            metadata: None,
            source_hash: None,
            source_path: None,
        })
        .unwrap();
    assert!(!id.is_empty());
    let list = s.list(&Filter::default()).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, id);
    assert_eq!(list[0].surface, "mcp-tool-call");
    assert!(list[0].content_hash.is_some());
}

#[test]
fn propose_without_content_has_no_hash() {
    let (_dir, s) = staged();
    s.propose(&ProposalInput {
        surface: "trails".into(),
        action: "waypoint_add".into(),
        target_path: "notes/raptor.md".into(),
        trail_id: Some("trail-abc".into()),
        content: None,
        metadata: None,
        source_hash: None,
        source_path: None,
    })
    .unwrap();
    let list = s.list(&Filter::default()).unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0].content_hash.is_none());
}

#[test]
fn list_filters_by_path() {
    let (_dir, s) = staged();
    s.propose(&ProposalInput {
        surface: "mcp-tool-call".into(),
        action: "write_note".into(),
        target_path: "notes/a.md".into(),
        trail_id: None,
        content: Some("a".into()),
        metadata: None,
        source_hash: None,
        source_path: None,
    })
    .unwrap();
    s.propose(&ProposalInput {
        surface: "mcp-tool-call".into(),
        action: "write_note".into(),
        target_path: "notes/b.md".into(),
        trail_id: None,
        content: Some("b".into()),
        metadata: None,
        source_hash: None,
        source_path: None,
    })
    .unwrap();

    let filtered = s
        .list(&Filter {
            path: Some("notes/a.md".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].target_path, "notes/a.md");
}

#[test]
fn list_filters_by_surface() {
    let (_dir, s) = staged();
    s.propose(&ProposalInput {
        surface: "mcp-tool-call".into(),
        action: "write_note".into(),
        target_path: "notes/a.md".into(),
        trail_id: None,
        content: Some("a".into()),
        metadata: None,
        source_hash: None,
        source_path: None,
    })
    .unwrap();
    s.propose(&ProposalInput {
        surface: "background-llm".into(),
        action: "write_note".into(),
        target_path: "notes/b.md".into(),
        trail_id: None,
        content: Some("b".into()),
        metadata: None,
        source_hash: None,
        source_path: None,
    })
    .unwrap();

    let filtered = s
        .list(&Filter {
            surface: Some("background-llm".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].surface, "background-llm");
}

#[test]
fn list_filters_by_trail_id() {
    let (_dir, s) = staged();
    s.propose(&ProposalInput {
        surface: "trails".into(),
        action: "trail_create".into(),
        target_path: "trails/new-trail.md".into(),
        trail_id: Some("t1".into()),
        content: None,
        metadata: None,
        source_hash: None,
        source_path: None,
    })
    .unwrap();
    s.propose(&ProposalInput {
        surface: "trails".into(),
        action: "waypoint_add".into(),
        target_path: "notes/x.md".into(),
        trail_id: Some("t2".into()),
        content: None,
        metadata: None,
        source_hash: None,
        source_path: None,
    })
    .unwrap();

    let filtered = s
        .list(&Filter {
            trail_id: Some("t1".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].trail_id.as_deref(), Some("t1"));
}

#[test]
fn list_filters_by_session_id_from_metadata() {
    let (_dir, s) = staged();
    s.propose(&ProposalInput {
        surface: "mcp-tool-call".into(),
        action: "write_note".into(),
        target_path: "notes/a.md".into(),
        trail_id: None,
        content: Some("a".into()),
        metadata: Some(serde_json::json!({"session_id": "s1"})),
        source_hash: None,
        source_path: None,
    })
    .unwrap();
    s.propose(&ProposalInput {
        surface: "mcp-tool-call".into(),
        action: "write_note".into(),
        target_path: "notes/b.md".into(),
        trail_id: None,
        content: Some("b".into()),
        metadata: Some(serde_json::json!({"session_id": "s2"})),
        source_hash: None,
        source_path: None,
    })
    .unwrap();

    let filtered = s
        .list(&Filter {
            session_id: Some("s1".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].target_path, "notes/a.md");
}

#[test]
fn count_returns_filtered_total() {
    let (_dir, s) = staged();
    for i in 0..5 {
        s.propose(&ProposalInput {
            surface: "batch-mutation".into(),
            action: "write_note".into(),
            target_path: format!("notes/{i}.md"),
            trail_id: None,
            content: Some("x".into()),
            metadata: None,
            source_hash: None,
            source_path: None,
        })
        .unwrap();
    }
    assert_eq!(s.count(&Filter::default()).unwrap(), 5);
    assert_eq!(
        s.count(&Filter {
            path: Some("notes/0.md".into()),
            ..Default::default()
        })
        .unwrap(),
        1
    );
    assert_eq!(
        s.count(&Filter {
            surface: Some("nonexistent".into()),
            ..Default::default()
        })
        .unwrap(),
        0
    );
}

#[test]
fn accept_writes_content_and_removes_from_pending() {
    let (dir, s) = staged();
    let vault = Vault::open(dir.path()).unwrap();
    vault.write_file("notes/a.md", "original").unwrap();

    let id = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("proposed".into()),
            metadata: None,
            source_hash: None,
            source_path: None,
        })
        .unwrap();

    let outcome = s.accept(&id, &vault, None).unwrap();
    assert_eq!(outcome.proposal_id, id);
    assert_eq!(outcome.target_path, "notes/a.md");
    assert!(!outcome.new_hash.is_empty());

    let (disk_content, _) = vault.read_file_with_hash("notes/a.md").unwrap();
    assert_eq!(disk_content, "proposed");

    assert!(s.list(&Filter::default()).unwrap().is_empty());
}

/// status: staging-action-move-note
#[test]
fn accept_move_note_renames_on_disk_and_records_renamed_change() {
    let (dir, s) = staged();
    let vault = Vault::open(dir.path()).unwrap();
    let changes = Changes::open(dir.path()).unwrap();
    vault.write_file("inbox/voyage.md", "embedding notes").unwrap();
    std::fs::create_dir_all(dir.path().join("research/embeddings")).unwrap();

    let id = s
        .propose(&ProposalInput {
            surface: "triage".into(),
            action: ACTION_MOVE_NOTE.into(),
            target_path: "research/embeddings/voyage.md".into(),
            trail_id: None,
            content: None,
            metadata: None,
            source_hash: None,
            source_path: Some("inbox/voyage.md".into()),
        })
        .unwrap();

    let outcome = s.accept(&id, &vault, Some(&changes)).unwrap();
    assert_eq!(outcome.proposal_id, id);
    assert_eq!(outcome.target_path, "research/embeddings/voyage.md");

    // file moved, proposal removed.
    assert!(!dir.path().join("inbox/voyage.md").exists());
    assert!(dir
        .path()
        .join("research/embeddings/voyage.md")
        .exists());
    assert!(s.list(&Filter::default()).unwrap().is_empty());

    // changes log captured the move with rename_from set.
    let rows = changes.history_for_path("research/embeddings/voyage.md", 10).unwrap();
    let renamed = rows
        .iter()
        .find(|r| matches!(r.op, ChangeOp::Renamed))
        .expect("expected a Renamed row");
    assert_eq!(renamed.rename_from.as_deref(), Some("inbox/voyage.md"));
}

/// status: staging-action-move-note
#[test]
fn accept_move_note_errors_when_source_missing() {
    let (dir, s) = staged();
    let vault = Vault::open(dir.path()).unwrap();
    let id = s
        .propose(&ProposalInput {
            surface: "triage".into(),
            action: ACTION_MOVE_NOTE.into(),
            target_path: "research/x.md".into(),
            trail_id: None,
            content: None,
            metadata: None,
            source_hash: None,
            source_path: Some("inbox/never-existed.md".into()),
        })
        .unwrap();
    // No file on disk → accept should refuse with the SourceMissing
    // reason carried in the error message.
    let err = s.accept(&id, &vault, None).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("source_missing"), "got {msg}");
}

/// status: staging-action-move-note
#[test]
fn accept_move_note_errors_when_target_occupied() {
    let (dir, s) = staged();
    let vault = Vault::open(dir.path()).unwrap();
    vault.write_file("a.md", "a").unwrap();
    vault.write_file("b.md", "b").unwrap();
    let id = s
        .propose(&ProposalInput {
            surface: "triage".into(),
            action: ACTION_MOVE_NOTE.into(),
            target_path: "b.md".into(),
            trail_id: None,
            content: None,
            metadata: None,
            source_hash: None,
            source_path: Some("a.md".into()),
        })
        .unwrap();
    let err = s.accept(&id, &vault, None).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("target_occupied"), "got {msg}");
}

#[test]
fn accept_metadata_only_removes_without_write() {
    let (_dir, s) = staged();
    let vault = Vault::open(_dir.path()).unwrap();
    let id = s
        .propose(&ProposalInput {
            surface: "trails".into(),
            action: "waypoint_add".into(),
            target_path: "notes/x.md".into(),
            trail_id: Some("t1".into()),
            content: None,
            metadata: None,
            source_hash: None,
            source_path: None,
        })
        .unwrap();

    let outcome = s.accept(&id, &vault, None).unwrap();
    assert_eq!(outcome.proposal_id, id);
    assert!(outcome.new_hash.is_empty());
    assert!(s.list(&Filter::default()).unwrap().is_empty());
}

#[test]
fn reject_removes_row() {
    let (_dir, s) = staged();
    let id = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("x".into()),
            metadata: None,
            source_hash: None,
            source_path: None,
        })
        .unwrap();

    s.reject(&id).unwrap();
    assert!(s.list(&Filter::default()).unwrap().is_empty());
}

#[test]
fn reject_nonexistent_returns_error() {
    let (_dir, s) = staged();
    match s.reject("nonexistent") {
        Err(Error::ProposalNotFound(_)) => {}
        other => panic!("expected ProposalNotFound, got {other:?}"),
    }
}

#[test]
fn accept_nonexistent_returns_error() {
    let (_dir, s) = staged();
    let vault = Vault::open(_dir.path()).unwrap();
    match s.accept("nonexistent", &vault, None) {
        Err(Error::ProposalNotFound(_)) => {}
        other => panic!("expected ProposalNotFound, got {other:?}"),
    }
}

#[test]
fn accept_all_batches_successes_and_skips_failures() {
    let (_dir, s) = staged();
    let vault = Vault::open(_dir.path()).unwrap();
    vault.write_file("notes/a.md", "orig-a").unwrap();
    vault.write_file("notes/b.md", "orig-b").unwrap();

    s.propose(&ProposalInput {
        surface: "mcp-tool-call".into(),
        action: "write_note".into(),
        target_path: "notes/a.md".into(),
        trail_id: None,
        content: Some("new-a".into()),
        metadata: None,
        source_hash: None,
        source_path: None,
    })
    .unwrap();
    s.propose(&ProposalInput {
        surface: "mcp-tool-call".into(),
        action: "write_note".into(),
        target_path: "notes/b.md".into(),
        trail_id: None,
        content: Some("new-b".into()),
        metadata: None,
        source_hash: None,
        source_path: None,
    })
    .unwrap();

    let outcomes = s
        .accept_all(&Filter::default(), &vault, None)
        .unwrap();
    assert_eq!(outcomes.len(), 2);
    assert!(s.list(&Filter::default()).unwrap().is_empty());
}

#[test]
fn gc_removes_old_proposals() {
    let (_dir, s) = staged();
    let id = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/old.md".into(),
            trail_id: None,
            content: Some("old".into()),
            metadata: None,
            source_hash: None,
            source_path: None,
        })
        .unwrap();
    // Backdate the row directly so the GC pass picks it up.
    {
        let conn = s.conn.lock().unwrap();
        conn.execute(
            "UPDATE proposals SET created_at_ms = 0 WHERE id = ?1",
            params![id],
        )
        .unwrap();
    }
    let removed = s.gc(1).unwrap();
    assert_eq!(removed, 1);
    assert!(s.list(&Filter::default()).unwrap().is_empty());
}

#[test]
fn gc_keeps_recent_proposals() {
    let (_dir, s) = staged();
    s.propose(&ProposalInput {
        surface: "mcp-tool-call".into(),
        action: "write_note".into(),
        target_path: "notes/recent.md".into(),
        trail_id: None,
        content: Some("recent".into()),
        metadata: None,
        source_hash: None,
        source_path: None,
    })
    .unwrap();
    let removed = s.gc(30).unwrap();
    assert_eq!(removed, 0);
    assert_eq!(s.list(&Filter::default()).unwrap().len(), 1);
}

#[test]
fn propose_then_accept_with_changes_log() {
    let dir = tempdir().unwrap();
    let s = Staging::open(dir.path()).unwrap();
    let vault = Vault::open(dir.path()).unwrap();
    vault.write_file("notes/a.md", "original").unwrap();
    let changes = Changes::open(dir.path()).unwrap();

    let id = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("proposed".into()),
            metadata: None,
            source_hash: None,
            source_path: None,
        })
        .unwrap();

    let outcome = s.accept(&id, &vault, Some(&changes)).unwrap();
    assert!(!outcome.new_hash.is_empty());

    // Two rows: the pre-write baseline + the user-accepted write.
    // `recent` is newest-first, so [0] is the write and [1] is the baseline.
    let rows = changes.recent(10).unwrap();
    assert_eq!(rows.len(), 2);
    let meta = &rows[0].metadata;
    assert_eq!(
        meta.get("staging_proposal_id").and_then(|v| v.as_str()),
        Some(id.as_str())
    );
    assert_eq!(meta.get("action").and_then(|v| v.as_str()), Some("write_note"));
    assert_eq!(meta.get("reviewed").and_then(serde_json::Value::as_bool), Some(true));
    assert_eq!(rows[0].author, "user");

    let baseline_meta = &rows[1].metadata;
    assert_eq!(
        baseline_meta.get("baseline").and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(rows[1].author, "user");
}

#[test]
fn accept_full_write_snapshots_baseline_for_existing_file() {
    let dir = tempdir().unwrap();
    let s = Staging::open(dir.path()).unwrap();
    let vault = Vault::open(dir.path()).unwrap();
    vault.write_file("notes/a.md", "original-body").unwrap();
    let changes = Changes::open(dir.path()).unwrap();

    let id = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("rewritten-body".into()),
            metadata: None,
            source_hash: None,
            source_path: None,
        })
        .unwrap();
    s.accept(&id, &vault, Some(&changes)).unwrap();

    // Rollback target must be the pre-write body, not None: there should
    // be a baseline row whose content captures the original body.
    let rows = changes.recent(10).unwrap();
    let baseline = rows
        .iter()
        .find(|r| {
            r.metadata
                .get("baseline")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .expect("baseline row should be present");
    let prior = changes
        .previous_content_for_path("notes/a.md", baseline.id + 1)
        .unwrap()
        .expect("baseline should provide a prior body");
    assert_eq!(prior.1, b"original-body");
}

#[test]
fn accept_edit_snapshots_baseline_for_existing_file() {
    let dir = tempdir().unwrap();
    let s = Staging::open(dir.path()).unwrap();
    let vault = Vault::open(dir.path()).unwrap();
    vault.write_file("notes/a.md", "hello foo world").unwrap();
    let changes = Changes::open(dir.path()).unwrap();

    let batch = s
        .propose_batch(&[EditProposalInput {
            surface: "mcp-tool-call".into(),
            action: "edit_note".into(),
            target_path: "notes/a.md".into(),
            edit: EditPayload {
                old_str: "foo".into(),
                new_str: "bar".into(),
                replace_all: false,
            },
            content: None,
            metadata: None,
            source_hash: None,
        }])
        .unwrap();
    s.accept(&batch.ids[0], &vault, Some(&changes)).unwrap();

    let rows = changes.recent(10).unwrap();
    let baseline = rows
        .iter()
        .find(|r| {
            r.metadata
                .get("baseline")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .expect("baseline row should be present");
    let prior = changes
        .previous_content_for_path("notes/a.md", baseline.id + 1)
        .unwrap()
        .expect("baseline should provide a prior body");
    assert_eq!(prior.1, b"hello foo world");
}

#[test]
fn accept_with_nulled_content_returns_missing_content() {
    let (_dir, s) = staged();
    let vault = Vault::open(_dir.path()).unwrap();
    vault.write_file("notes/a.md", "original").unwrap();

    let id = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("proposed".into()),
            metadata: None,
            source_hash: None,
            source_path: None,
        })
        .unwrap();
    // Simulate corruption: row claims a content_hash but the BLOB has been wiped.
    {
        let conn = s.conn.lock().unwrap();
        conn.execute(
            "UPDATE proposals SET content = NULL WHERE id = ?1",
            params![id],
        )
        .unwrap();
    }

    match s.accept(&id, &vault, None) {
        Err(Error::MissingContent(_)) => {}
        other => panic!("expected MissingContent, got {other:?}"),
    }
}

#[test]
fn accept_with_tampered_content_detects_integrity_failure() {
    let (_dir, s) = staged();
    let vault = Vault::open(_dir.path()).unwrap();
    vault.write_file("notes/a.md", "original").unwrap();

    let id = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("proposed".into()),
            metadata: None,
            source_hash: None,
            source_path: None,
        })
        .unwrap();
    // Swap the BLOB for a different (but valid zstd) frame so the
    // decoded hash no longer matches the stored content_hash.
    let tampered = zstd::encode_all(&b"tampered"[..], super::types::ZSTD_LEVEL).unwrap();
    {
        let conn = s.conn.lock().unwrap();
        conn.execute(
            "UPDATE proposals SET content = ?1 WHERE id = ?2",
            params![tampered, id],
        )
        .unwrap();
    }

    match s.accept(&id, &vault, None) {
        Err(Error::DiskDrift { .. }) => {}
        other => panic!("expected DiskDrift, got {other:?}"),
    }
}

#[test]
fn accept_create_action_works_when_file_does_not_exist() {
    let dir = tempdir().unwrap();
    let s = Staging::open(dir.path()).unwrap();
    let vault = Vault::open(dir.path()).unwrap();

    let proposed = "# New note";
    let id = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/new.md".into(),
            trail_id: None,
            content: Some(proposed.into()),
            metadata: None,
            source_hash: None,
            source_path: None,
        })
        .unwrap();

    let outcome = s.accept(&id, &vault, None).unwrap();
    assert!(!outcome.new_hash.is_empty());

    let (content, _) = vault.read_file_with_hash("notes/new.md").unwrap();
    assert_eq!(content, proposed);
}

#[test]
fn propose_batch_assigns_shared_batch_id_and_per_edit_payloads() {
    let (_dir, s) = staged();
    let inputs = vec![
        EditProposalInput {
            surface: "mcp-tool-call".into(),
            action: "edit_note".into(),
            target_path: "notes/a.md".into(),
            edit: EditPayload {
                old_str: "foo".into(),
                new_str: "bar".into(),
                replace_all: false,
            },
            content: Some("bar".into()),
            metadata: None,
            source_hash: None,
        },
        EditProposalInput {
            surface: "mcp-tool-call".into(),
            action: "edit_note".into(),
            target_path: "notes/a.md".into(),
            edit: EditPayload {
                old_str: "baz".into(),
                new_str: "qux".into(),
                replace_all: false,
            },
            content: Some("qux".into()),
            metadata: None,
            source_hash: None,
        },
    ];
    let outcome = s.propose_batch(&inputs).unwrap();
    assert_eq!(outcome.ids.len(), 2);
    let list = s.list(&Filter::default()).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].batch_id.as_deref(), Some(outcome.batch_id.as_str()));
    assert_eq!(list[1].batch_id.as_deref(), Some(outcome.batch_id.as_str()));
    assert!(list[0].edit.is_some());
    assert_eq!(list[0].edit.as_ref().unwrap().old_str, "foo");
}

#[test]
fn accept_edit_row_reanchors_against_current_disk() {
    let (_dir, s) = staged();
    let vault = Vault::open(_dir.path()).unwrap();
    vault.write_file("notes/a.md", "hello foo world").unwrap();
    let outcome = s
        .propose_batch(&[EditProposalInput {
            surface: "mcp-tool-call".into(),
            action: "edit_note".into(),
            target_path: "notes/a.md".into(),
            edit: EditPayload {
                old_str: "foo".into(),
                new_str: "bar".into(),
                replace_all: false,
            },
            content: Some("bar".into()),
            metadata: None,
            source_hash: None,
        }])
        .unwrap();
    let id = &outcome.ids[0];
    let accepted = s.accept(id, &vault, None).unwrap();
    assert!(!accepted.new_hash.is_empty());
    let (after, _) = vault.read_file_with_hash("notes/a.md").unwrap();
    assert_eq!(after, "hello bar world");
}

#[test]
fn accept_edit_row_returns_anchor_conflict_when_old_str_missing() {
    let (dir, s) = staged();
    let vault = Vault::open(dir.path()).unwrap();
    vault.write_file("notes/a.md", "hello world").unwrap();
    let outcome = s
        .propose_batch(&[EditProposalInput {
            surface: "mcp-tool-call".into(),
            action: "edit_note".into(),
            target_path: "notes/a.md".into(),
            edit: EditPayload {
                old_str: "missing".into(),
                new_str: "x".into(),
                replace_all: false,
            },
            content: Some("x".into()),
            metadata: None,
            source_hash: None,
        }])
        .unwrap();
    let id = &outcome.ids[0];
    match s.accept(id, &vault, None) {
        Err(Error::AnchorConflict(_)) => {}
        other => panic!("expected AnchorConflict, got {other:?}"),
    }
}

#[test]
fn apply_edit_replaces_unique_match() {
    let out = apply_edit(
        "hello foo world",
        &EditPayload {
            old_str: "foo".into(),
            new_str: "BAR".into(),
            replace_all: false,
        },
    )
    .unwrap();
    assert_eq!(out, "hello BAR world");
}

#[test]
fn apply_edit_rejects_multiple_matches_without_replace_all() {
    let res = apply_edit(
        "foo foo",
        &EditPayload {
            old_str: "foo".into(),
            new_str: "x".into(),
            replace_all: false,
        },
    );
    assert!(matches!(res, Err(Error::AnchorConflict(_))));
}

#[test]
fn apply_edit_replace_all_swaps_every_match() {
    let out = apply_edit(
        "foo foo bar",
        &EditPayload {
            old_str: "foo".into(),
            new_str: "x".into(),
            replace_all: true,
        },
    )
    .unwrap();
    assert_eq!(out, "x x bar");
}

// ── staging-proposal-state / staging-drift-eager-recheck ──

#[test]
fn recheck_edit_row_stays_applyable_when_anchor_still_unique() {
    let (_dir, s) = staged();
    let outcome = s
        .propose_batch(&[EditProposalInput {
            surface: "mcp-tool-call".into(),
            action: "edit_note".into(),
            target_path: "notes/a.md".into(),
            edit: EditPayload {
                old_str: "foo".into(),
                new_str: "bar".into(),
                replace_all: false,
            },
            content: Some("bar".into()),
            metadata: None,
            source_hash: None,
        }])
        .unwrap();
    let id = &outcome.ids[0];
    let st = s.recheck(id, Some("hello foo world")).unwrap();
    assert_eq!(st.new_state, ProposalState::Applyable);
    let p = &s.list(&Filter::default()).unwrap()[0];
    assert_eq!(p.state, ProposalState::Applyable);
    assert!(p.conflict_reason.is_none());
}

#[test]
fn recheck_edit_row_flips_to_anchor_missing() {
    let (_dir, s) = staged();
    let outcome = s
        .propose_batch(&[EditProposalInput {
            surface: "mcp-tool-call".into(),
            action: "edit_note".into(),
            target_path: "notes/a.md".into(),
            edit: EditPayload {
                old_str: "foo".into(),
                new_str: "bar".into(),
                replace_all: false,
            },
            content: Some("bar".into()),
            metadata: None,
            source_hash: None,
        }])
        .unwrap();
    let id = &outcome.ids[0];
    let st = s.recheck(id, Some("nothing here")).unwrap();
    assert_eq!(st.new_state, ProposalState::Conflicted);
    let p = &s.list(&Filter::default()).unwrap()[0];
    assert_eq!(p.conflict_reason, Some(ConflictReason::AnchorMissing));
}

#[test]
fn recheck_edit_row_flips_to_anchor_not_unique() {
    let (_dir, s) = staged();
    let outcome = s
        .propose_batch(&[EditProposalInput {
            surface: "mcp-tool-call".into(),
            action: "edit_note".into(),
            target_path: "notes/a.md".into(),
            edit: EditPayload {
                old_str: "foo".into(),
                new_str: "bar".into(),
                replace_all: false,
            },
            content: Some("bar".into()),
            metadata: None,
            source_hash: None,
        }])
        .unwrap();
    let id = &outcome.ids[0];
    let st = s.recheck(id, Some("foo and foo again")).unwrap();
    assert_eq!(st.new_state, ProposalState::Conflicted);
    let p = &s.list(&Filter::default()).unwrap()[0];
    assert_eq!(p.conflict_reason, Some(ConflictReason::AnchorNotUnique));
}

#[test]
fn recheck_edit_row_target_missing_when_disk_none() {
    let (_dir, s) = staged();
    let outcome = s
        .propose_batch(&[EditProposalInput {
            surface: "mcp-tool-call".into(),
            action: "edit_note".into(),
            target_path: "notes/a.md".into(),
            edit: EditPayload {
                old_str: "foo".into(),
                new_str: "bar".into(),
                replace_all: false,
            },
            content: Some("bar".into()),
            metadata: None,
            source_hash: None,
        }])
        .unwrap();
    let id = &outcome.ids[0];
    let st = s.recheck(id, None).unwrap();
    assert_eq!(st.new_state, ProposalState::Conflicted);
    let p = &s.list(&Filter::default()).unwrap()[0];
    assert_eq!(p.conflict_reason, Some(ConflictReason::TargetMissing));
}

#[test]
fn recheck_write_row_applyable_when_hash_unchanged() {
    let (_dir, s) = staged();
    let propose_time = "original content";
    let source = hash_string(propose_time);
    let id = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("proposed".into()),
            metadata: None,
            source_hash: Some(source),
            source_path: None,
        })
        .unwrap();
    let st = s.recheck(&id, Some(propose_time)).unwrap();
    assert_eq!(st.new_state, ProposalState::Applyable);
}

#[test]
fn recheck_write_row_flips_on_hash_changed() {
    let (_dir, s) = staged();
    let source = hash_string("original");
    let id = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("proposed".into()),
            metadata: None,
            source_hash: Some(source),
            source_path: None,
        })
        .unwrap();
    let st = s.recheck(&id, Some("drifted")).unwrap();
    assert_eq!(st.new_state, ProposalState::Conflicted);
    let p = &s.list(&Filter::default()).unwrap()[0];
    assert_eq!(p.conflict_reason, Some(ConflictReason::HashChanged));
}

#[test]
fn recheck_create_row_applyable_while_target_absent() {
    let (_dir, s) = staged();
    let id = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/new.md".into(),
            trail_id: None,
            content: Some("# New".into()),
            metadata: None,
            source_hash: None,
            source_path: None,
        })
        .unwrap();
    let st = s.recheck(&id, None).unwrap();
    assert_eq!(st.new_state, ProposalState::Applyable);
    let st2 = s.recheck(&id, Some("someone wrote here first")).unwrap();
    assert_eq!(st2.new_state, ProposalState::Conflicted);
}

#[test]
fn recheck_transition_broadcasts_changed_event() {
    let (_dir, s) = staged();
    let mut rx = s.subscribe();
    let id = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("proposed".into()),
            metadata: None,
            source_hash: Some(hash_string("original")),
            source_path: None,
        })
        .unwrap();
    let _ = rx.try_recv();

    s.recheck(&id, Some("drifted")).unwrap();
    assert!(rx.try_recv().is_ok(), "transition should broadcast");

    s.recheck(&id, Some("drifted")).unwrap();
    assert!(
        rx.try_recv().is_err(),
        "idempotent recheck should not broadcast"
    );
}

#[test]
fn list_filters_by_state() {
    let (_dir, s) = staged();
    let id_a = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("a".into()),
            metadata: None,
            source_hash: Some(hash_string("orig")),
            source_path: None,
        })
        .unwrap();
    let _id_b = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/b.md".into(),
            trail_id: None,
            content: Some("b".into()),
            metadata: None,
            source_hash: Some(hash_string("orig")),
            source_path: None,
        })
        .unwrap();
    s.recheck(&id_a, Some("drifted")).unwrap();
    let applyable = s
        .list(&Filter {
            state: Some(ProposalState::Applyable),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(applyable.len(), 1);
    let conflicted = s
        .list(&Filter {
            state: Some(ProposalState::Conflicted),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(conflicted.len(), 1);
    assert_eq!(conflicted[0].id, id_a);
}

#[test]
fn content_round_trips_through_zstd() {
    let (_dir, s) = staged();
    let body = "# Big note\n\n".repeat(50);
    let id = s
        .propose(&ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some(body.clone()),
            metadata: None,
            source_hash: None,
            source_path: None,
        })
        .unwrap();
    assert_eq!(s.content(&id).unwrap(), body);
}
