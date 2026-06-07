//! Tests for the op-log substrate: materialization round-trips (frontmatter
//! verbatim), pending stage/accept/reject, pending-survives-restart, drift
//! detection, the `pending_view = accepted + pending` property, the
//! frontmatter-fence detection helper, the author wire round-trip, and the
//! side-table query/status states. The working-layer + scenario tests live in
//! the [`working_layer`] submodule; `user_ctx` below is shared with it.

mod sync;
mod working_layer;

use super::shapes::{Author, OpKind};
use super::*;
use tempfile::tempdir;

fn user_ctx() -> ProducerCtx {
    ProducerCtx {
        author: Author::Agent("claude-code".to_string()),
        surface: "mcp-tool-call".to_string(),
        session_id: Some("sess-1".to_string()),
    }
}

const FRONTMATTER_DOC: &str = "---\ntitle: My Note\ntags: [a, b]\n# a comment\ndate: 2026-05-22\n---\n\n# Heading\n\nBody text here.\n";

#[test]
fn materialize_round_trips_verbatim_with_frontmatter() {
    // status: op-log-materialization
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("notes/a.md", "note", FRONTMATTER_DOC, &Author::User)
        .unwrap();
    let got = log.materialize_accepted(&doc_id).unwrap();
    assert_eq!(got.text, FRONTMATTER_DOC, "materialize must be byte-identical");
    assert!(!got.tombstone);
    // The on-disk .md equals materialize(accepted) by construction.
    let on_disk = std::fs::read_to_string(dir.path().join("notes/a.md")).unwrap();
    assert_eq!(on_disk, FRONTMATTER_DOC);
}

#[test]
fn content_write_resurrects_tombstoned_path() {
    // status: op-log-atomic-write
    // Delete a doc (tombstone + keep the path → doc_id mapping), then write
    // fresh content to the same path — the re-create that `user_save`/
    // `doc_id_or_seed` routes to the existing tombstoned doc. The write must
    // resurrect the doc so the new `.md` lands on disk instead of being
    // suppressed as a tombstoned write.
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "original\n", &Author::User)
        .unwrap();
    log.tombstone_document(&doc_id, &Author::User).unwrap();
    assert!(log.materialize_accepted(&doc_id).unwrap().tombstone);

    // Re-create at the same path: the producer seam resolves the same
    // (tombstoned) doc, then commits the new content as a user write.
    log.apply_user_text(&doc_id, "reborn\n").unwrap();

    let got = log.materialize_accepted(&doc_id).unwrap();
    assert!(!got.tombstone, "content write must clear the tombstone");
    assert_eq!(got.text, "reborn\n");
    let on_disk = std::fs::read_to_string(dir.path().join("a.md")).unwrap();
    assert_eq!(on_disk, "reborn\n", "the resurrected .md must be written");
}

#[test]
fn frontmatter_keys_comments_scalars_survive_byte_for_byte() {
    // Reordered keys / comments / unusual scalars are all plain text inside
    // the single text run — no YAML re-emit can touch them.
    // status: op-log-document-shape
    let weird = "---\nzeta: 1\nalpha: \"quoted\"\n# keep me\nnested:\n  - x\n  - y\nbool_as_str: 'true'\n---\nbody\n";
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("w.md", "note", weird, &Author::User)
        .unwrap();
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, weird);
}

#[test]
fn frontmatter_fence_detection() {
    // status: op-log-op-shape
    use super::shapes::{frontmatter_fence_end, is_frontmatter_range};
    let fence_end = frontmatter_fence_end(FRONTMATTER_DOC).unwrap();
    // The closing fence + newline ends right before the blank line + heading.
    assert_eq!(&FRONTMATTER_DOC[fence_end..fence_end + 1], "\n");
    // A range inside the frontmatter is detected.
    let title_pos = FRONTMATTER_DOC.find("My Note").unwrap();
    assert!(is_frontmatter_range(
        FRONTMATTER_DOC,
        title_pos,
        title_pos + "My Note".len()
    ));
    // A range in the body is not.
    let body_pos = FRONTMATTER_DOC.find("Body").unwrap();
    assert!(!is_frontmatter_range(
        FRONTMATTER_DOC,
        body_pos,
        body_pos + 4
    ));
    // No frontmatter at all → no range is ever frontmatter.
    assert!(frontmatter_fence_end("# just a heading\n").is_none());
    assert!(!is_frontmatter_range("# just a heading\n", 0, 3));
}

#[test]
fn set_frontmatter_label_for_in_fence_edit() {
    // An anchored replace whose old_str lands in the fence is labeled
    // SetFrontmatter; a body edit is a Replace.
    // status: op-log-op-shape
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", FRONTMATTER_DOC, &Author::User)
        .unwrap();
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec {
                old_str: Some("My Note".to_string()),
                new_str: "Renamed".to_string(),
            }],
            &user_ctx(),
        )
        .unwrap();
    let pending = log.pending_ops(&doc_id).unwrap();
    let staged = pending.iter().find(|p| p.op_id == out.op_ids[0]).unwrap();
    assert!(matches!(staged.op_kind, OpKind::SetFrontmatter));

    let out2 = log
        .stage_pending(
            &doc_id,
            &[EditSpec {
                old_str: Some("Body text here.".to_string()),
                new_str: "Changed body.".to_string(),
            }],
            &user_ctx(),
        )
        .unwrap();
    let pending = log.pending_ops(&doc_id).unwrap();
    let staged = pending.iter().find(|p| p.op_id == out2.op_ids[0]).unwrap();
    assert!(matches!(staged.op_kind, OpKind::Replace { anchor: Some(_) }));
}

/// Stage a whole-document rewrite to `to` and accept it, returning the
/// accepted op id (which equals the pending op id). Used by the history tests.
fn stage_accept(log: &OpLog, doc_id: &str, to: &str) -> String {
    let out = log
        .stage_pending(
            doc_id,
            &[EditSpec { old_str: None, new_str: to.to_string() }],
            &user_ctx(),
        )
        .unwrap();
    let op = out.op_ids[0].clone();
    log.accept_pending(doc_id, &op).unwrap();
    op
}

#[test]
fn history_reconstructs_across_keyframes_and_reopen() {
    // status: op-log-accepted-op-retention
    // More than KEYFRAME_INTERVAL (16) accepted edits, so `.ops` spans several
    // keyframes with delta frames between. Every version must reconstruct via
    // the keyframe walk-back — including after a reopen (which forces a fresh
    // keyframe on the next write, re-anchoring the delta chain).
    let dir = tempdir().unwrap();
    let doc_id;
    let mut versions: Vec<(String, String)> = Vec::new();
    {
        let log = OpLog::open(dir.path()).unwrap();
        doc_id = log.create_document("a.md", "note", "v0\n", &Author::User).unwrap();
        for i in 1..25 {
            let text = format!("version {i}\nbody line {i}\n");
            let op = stage_accept(&log, &doc_id, &text);
            versions.push((op, text));
        }
    }
    // Reopen → the next write is forced to a keyframe; keep editing across it.
    let log = OpLog::open(dir.path()).unwrap();
    for i in 25..32 {
        let text = format!("version {i}\nbody line {i}\n");
        let op = stage_accept(&log, &doc_id, &text);
        versions.push((op, text));
    }
    // Every retained version reconstructs byte-for-byte.
    for (op, text) in &versions {
        assert_eq!(
            log.materialize_at(&doc_id, op).unwrap().unwrap().text,
            *text,
            "version at op {op} must reconstruct from its keyframe + deltas",
        );
    }
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "version 31\nbody line 31\n");
}

#[test]
fn materialize_at_reconstructs_each_accepted_version() {
    // status: op-log-history-materialization
    // status: op-log-accepted-op-retention
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log.create_document("a.md", "note", "v1\n", &Author::User).unwrap();

    let a = stage_accept(&log, &doc_id, "v2\n");
    let b = stage_accept(&log, &doc_id, "v3\n");

    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "v3\n");
    assert_eq!(log.materialize_at(&doc_id, &a).unwrap().unwrap().text, "v2\n");
    assert_eq!(log.materialize_at(&doc_id, &b).unwrap().unwrap().text, "v3\n");

    // The seed version is reconstructable from the create content op.
    let hist = log.doc_history(&doc_id, 50).unwrap();
    let seed_reconstructable = hist.iter().any(|r| {
        log.materialize_at(&doc_id, &r.op_id)
            .ok()
            .flatten()
            .map(|c| c.text)
            == Some("v1\n".to_string())
    });
    assert!(seed_reconstructable, "seed version v1 should be reconstructable");

    // An unknown op id yields None, not an error.
    assert!(log
        .materialize_at(&doc_id, "01ZZZZZZZZZZZZZZZZZZZZZZZZZ")
        .unwrap()
        .is_none());
}

#[test]
fn materialize_at_survives_restart() {
    // status: op-log-accepted-op-retention
    let dir = tempdir().unwrap();
    let a = {
        let log = OpLog::open(dir.path()).unwrap();
        let doc_id = log.create_document("a.md", "note", "v1\n", &Author::User).unwrap();
        stage_accept(&log, &doc_id, "v2\n")
    };
    // Reopen: the `.ops` history log is reloaded from disk.
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log.doc_id_for_path("a.md").unwrap().unwrap();
    assert_eq!(log.materialize_at(&doc_id, &a).unwrap().unwrap().text, "v2\n");
}

#[test]
fn materialize_at_records_tombstone_transition() {
    // status: op-log-history-materialization
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log.create_document("a.md", "note", "body\n", &Author::User).unwrap();
    let edited = stage_accept(&log, &doc_id, "body edited\n");
    log.tombstone_document(&doc_id, &Author::User).unwrap();

    // The pre-tombstone op reconstructs live content.
    let pre = log.materialize_at(&doc_id, &edited).unwrap().unwrap();
    assert!(!pre.tombstone);
    assert_eq!(pre.text, "body edited\n");

    // The tombstone op reconstructs the deleted state (found by its
    // reconstructed flag — create/edit/tombstone share a millisecond timestamp,
    // so newest-first ordering can't be relied on to pick it out).
    let hist = log.doc_history(&doc_id, 50).unwrap();
    let tomb_reconstructs = hist.iter().any(|r| {
        log.materialize_at(&doc_id, &r.op_id)
            .ok()
            .flatten()
            .map(|c| c.tombstone)
            == Some(true)
    });
    assert!(tomb_reconstructs, "tombstone state should be reconstructable from history");
}

#[test]
fn rename_repoints_index_and_drops_old_path() {
    // status: op-log-op-shape
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log.create_document("a.md", "note", "body\n", &Author::User).unwrap();
    log.rename_document(&doc_id, "b.md", &Author::User).unwrap();
    // The id IS the path: a rename relabels the doc, so the old path no longer
    // resolves and the new path resolves to itself (path-as-identity).
    assert_eq!(log.doc_id_for_path("a.md").unwrap(), None, "old path mapping not dropped");
    assert_eq!(
        log.doc_id_for_path("b.md").unwrap().as_deref(),
        Some("b.md"),
        "new path not mapped to the doc"
    );
    // Content is unchanged by the rename; the doc now lives at its new path.
    assert_eq!(log.materialize_accepted("b.md").unwrap().text, "body\n");
}

#[test]
fn tombstone_records_op_and_keeps_path_resolvable() {
    // status: op-log-op-shape
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log.create_document("a.md", "note", "body\n", &Author::User).unwrap();
    log.tombstone_document(&doc_id, &Author::User).unwrap();
    // The doc reads as tombstoned and stays resolvable both ways so the
    // history / activity feed can still surface the deletion by path.
    assert!(log.materialize_accepted(&doc_id).unwrap().tombstone);
    assert_eq!(log.doc_id_for_path("a.md").unwrap().as_deref(), Some(doc_id.as_str()));
    assert_eq!(log.path_for_doc(&doc_id).unwrap().as_deref(), Some("a.md"));
}

#[test]
fn chained_agent_edits_anchor_on_prior_pending() {
    // status: op-log-agent-replica
    // A follow-up edit anchors on text the agent staged in a prior, not-yet-
    // accepted edit. The anchor lives only in the session's pending view, not
    // in `accepted`, so the producer must resolve against the pending view.
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log.create_document("a.md", "note", "hello world\n", &Author::User).unwrap();
    // Edit 1: world -> earth (resolves against accepted).
    log.stage_pending(
        &doc_id,
        &[EditSpec { old_str: Some("world".into()), new_str: "earth".into() }],
        &user_ctx(),
    )
    .unwrap();
    // Edit 2: anchor on "earth" — present only in the pending view.
    let out2 = log
        .stage_pending(
            &doc_id,
            &[EditSpec { old_str: Some("earth".into()), new_str: "mars".into() }],
            &user_ctx(),
        )
        .unwrap();
    // Both edits compose in the session's pending view.
    assert_eq!(
        log.materialize_pending_view(&doc_id, Some("sess-1")).unwrap().text,
        "hello mars\n"
    );
    // The chained op is not falsely flagged as drifted.
    assert!(!log.is_pending_drifted(&doc_id, &out2.op_ids[0]).unwrap());
    // accepted stays untouched until accept.
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "hello world\n");
    // Accepting both in order lands the composed result on disk.
    let out1_ops = log.pending_ops(&doc_id).unwrap();
    for op in &out1_ops {
        log.accept_pending(&doc_id, &op.op_id).unwrap();
    }
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "hello mars\n");
}

#[test]
fn pending_view_equals_accepted_plus_pending() {
    // status: op-log-two-doc-model
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "hello world\n", &Author::User)
        .unwrap();
    log.stage_pending(
        &doc_id,
        &[EditSpec {
            old_str: Some("world".to_string()),
            new_str: "earth".to_string(),
        }],
        &user_ctx(),
    )
    .unwrap();
    // accepted is unchanged by staging.
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "hello world\n");
    // pending_view shows the staged edit applied on top.
    let view = log.materialize_pending_view(&doc_id, Some("sess-1")).unwrap();
    assert_eq!(view.text, "hello earth\n");
}

#[test]
fn accept_applies_to_accepted_and_disk() {
    // status: op-log-status-states
    // status: op-log-atomic-write
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "hello world\n", &Author::User)
        .unwrap();
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec {
                old_str: Some("world".to_string()),
                new_str: "earth".to_string(),
            }],
            &user_ctx(),
        )
        .unwrap();
    log.accept_pending(&doc_id, &out.op_ids[0]).unwrap();
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "hello earth\n");
    assert!(log.pending_ops(&doc_id).unwrap().is_empty());
    let on_disk = std::fs::read_to_string(dir.path().join("a.md")).unwrap();
    assert_eq!(on_disk, "hello earth\n");
    // An accepted side-table row exists for the agent.
    let rows = log
        .query_metadata(&meta::Filter {
            author_class: Some("agent".to_string()),
            status: Some(meta::OpStatus::Accepted),
            ..Default::default()
        })
        .unwrap();
    assert!(rows.iter().any(|r| matches!(r.author, Author::Agent(_))));
}

#[test]
fn reject_discards_and_leaves_no_history_row() {
    // A rejected pending edit is transient editorial state: it leaves NO
    // durable history row (the regenerable `op_history` index rebuilds from
    // `.ops`, which only holds accepted history). Rejection is observable via
    // the pending edit disappearing, not via a history row.
    // status: op-log-status-states
    // status: op-log-no-oplog-db
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "hello world\n", &Author::User)
        .unwrap();
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec {
                old_str: Some("world".to_string()),
                new_str: "earth".to_string(),
            }],
            &user_ctx(),
        )
        .unwrap();
    // Before rejection: exactly one pending edit, one accepted (Create) row.
    assert_eq!(log.pending_ops(&doc_id).unwrap().len(), 1);
    let accepted_before = log.doc_history(&doc_id, 50).unwrap().len();
    log.reject_pending(&doc_id, &out.op_ids[0]).unwrap();
    // accepted untouched, queue emptied — the rejection is observable here.
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "hello world\n");
    assert!(log.pending_ops(&doc_id).unwrap().is_empty());
    // No rejected query is possible (the status is a no-op filter) and the
    // accepted history is unchanged — no audit row was written.
    let rejected = log
        .query_metadata(&meta::Filter {
            status: Some(meta::OpStatus::Rejected),
            ..Default::default()
        })
        .unwrap();
    assert!(rejected.is_empty());
    assert_eq!(log.doc_history(&doc_id, 50).unwrap().len(), accepted_before);
}

#[test]
fn pending_survives_restart() {
    // status: op-log-pending-survives-restart
    let dir = tempdir().unwrap();
    let doc_id;
    let op_id;
    {
        let log = OpLog::open(dir.path()).unwrap();
        doc_id = log
            .create_document("a.md", "note", "hello world\n", &Author::User)
            .unwrap();
        let out = log
            .stage_pending(
                &doc_id,
                &[EditSpec {
                    old_str: Some("world".to_string()),
                    new_str: "earth".to_string(),
                }],
                &user_ctx(),
            )
            .unwrap();
        op_id = out.op_ids[0].clone();
    }
    // Reopen from disk — pending op must still be there and applyable.
    let log = OpLog::open(dir.path()).unwrap();
    let pending = log.pending_ops(&doc_id).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].op_id, op_id);
    log.accept_pending(&doc_id, &op_id).unwrap();
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "hello earth\n");
}

#[test]
fn drift_detected_when_accepted_advances() {
    // status: op-log-pending-queue
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "hello world\n", &Author::User)
        .unwrap();
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec {
                old_str: Some("world".to_string()),
                new_str: "earth".to_string(),
            }],
            &user_ctx(),
        )
        .unwrap();
    // Not drifted initially.
    assert!(!log.is_pending_drifted(&doc_id, &out.op_ids[0]).unwrap());
    // The user deletes the whole line the agent's edit anchored on.
    let len = "hello world\n".len();
    log.apply_user_edit(&doc_id, 0, len, "totally different\n").unwrap();
    // The pending op's intended "earth" content can no longer land.
    assert!(log.is_pending_drifted(&doc_id, &out.op_ids[0]).unwrap());
}

#[test]
fn anchor_conflict_on_missing_old_str() {
    // status: op-log-pending-queue
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "hello world\n", &Author::User)
        .unwrap();
    let err = log
        .stage_pending(
            &doc_id,
            &[EditSpec {
                old_str: Some("nonexistent".to_string()),
                new_str: "x".to_string(),
            }],
            &user_ctx(),
        )
        .unwrap_err();
    assert!(matches!(err, error::Error::Anchor(_)));
}

#[test]
fn whole_body_rewrite_replaces_whole_file_without_duplicating_frontmatter() {
    // `write_note` content is the FULL file (frontmatter + body). Accepting a
    // whole-document rewrite must replace the entire `text`, not append the new
    // content after the existing frontmatter fence (which duplicated it).
    // status: op-log-pending-queue
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", FRONTMATTER_DOC, &Author::User)
        .unwrap();
    // A full rewrite: same frontmatter, new body — exactly what an agent's
    // write_note (which carries the whole file) sends.
    let new_doc =
        "---\ntitle: My Note\ntags: [a, b]\n# a comment\ndate: 2026-05-22\n---\n\n# Heading\n\nBrand new body.\n";
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec {
                old_str: None,
                new_str: new_doc.to_string(),
            }],
            &user_ctx(),
        )
        .unwrap();
    log.accept_pending(&doc_id, &out.op_ids[0]).unwrap();
    let got = log.materialize_accepted(&doc_id).unwrap().text;
    assert_eq!(got, new_doc, "whole-file rewrite should equal the new doc");
    assert_eq!(
        got.matches("title: My Note").count(),
        1,
        "frontmatter duplicated: {got}"
    );

    // A no-op rewrite (identical content) stages nothing.
    let noop = log
        .stage_pending(
            &doc_id,
            &[EditSpec {
                old_str: None,
                new_str: new_doc.to_string(),
            }],
            &user_ctx(),
        )
        .unwrap();
    assert!(
        noop.op_ids.is_empty(),
        "unchanged whole-document rewrite should stage no op"
    );
}

#[test]
fn author_wire_round_trips() {
    // status: op-log-author-classes
    for a in [
        Author::User,
        Author::Agent("claude-code".to_string()),
        Author::External,
        Author::Extractor("pdf".to_string()),
        Author::Auto("triage".to_string()),
        Author::Sync("phone".to_string()),
    ] {
        let wire = a.as_wire();
        assert_eq!(Author::parse(&wire), a, "round trip failed for {wire}");
    }
    assert_eq!(Author::Agent("x".to_string()).class(), "agent");
}

#[test]
fn query_filters_by_author_class_and_doc() {
    // status: op-log-side-table
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "hello\n", &Author::User)
        .unwrap();
    // The Create + content Replace seed rows are author=user.
    let user_rows = log
        .query_metadata(&meta::Filter {
            author_class: Some("user".to_string()),
            ..Default::default()
        })
        .unwrap();
    assert!(!user_rows.is_empty());
    assert!(user_rows.iter().all(|r| matches!(r.author, Author::User)));
    // doc filter scopes to this doc.
    let doc_rows = log
        .query_metadata(&meta::Filter {
            doc_id: Some(doc_id.clone()),
            ..Default::default()
        })
        .unwrap();
    assert!(doc_rows.iter().all(|r| r.doc_id == doc_id));
}

#[test]
fn doc_index_maps_path_to_id() {
    // status: op-log-store-layout
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("sub/dir/note.md", "note", "x\n", &Author::User)
        .unwrap();
    assert_eq!(
        log.doc_id_for_path("sub/dir/note.md").unwrap(),
        Some(doc_id)
    );
    assert_eq!(log.doc_id_for_path("missing.md").unwrap(), None);
}

#[test]
fn gc_trims_old_accepted_history_rows() {
    // GC trims the regenerable `op_history` query-index (the activity-retention
    // window). A `Rejected` GC is a no-op (no rejected rows are ever stored —
    // a rejected pending edit leaves no frame); an `Accepted` GC with a future
    // cutoff trims the accepted history rows.
    // status: op-log-status-states
    // status: op-log-no-oplog-db
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "hello world\n", &Author::User)
        .unwrap();
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec {
                old_str: Some("world".to_string()),
                new_str: "earth".to_string(),
            }],
            &user_ctx(),
        )
        .unwrap();
    log.reject_pending(&doc_id, &out.op_ids[0]).unwrap();
    // Rejecting wrote no row, so a Rejected GC deletes nothing.
    assert_eq!(log.gc_metadata(meta::OpStatus::Rejected, i64::MAX).unwrap(), 0);
    // The one accepted (Create) row is present; an Accepted GC with a future
    // cutoff trims it.
    assert_eq!(log.doc_history(&doc_id, 50).unwrap().len(), 1);
    let deleted = log.gc_metadata(meta::OpStatus::Accepted, i64::MAX).unwrap();
    assert_eq!(deleted, 1);
    assert!(log.doc_history(&doc_id, 50).unwrap().is_empty());
}

#[test]
fn op_history_index_is_regenerable_from_ops() {
    // The `op_history` query-index is a REGENERABLE cache over the durable
    // `.ops` frames (`changes-query-api` / `op-log-no-oplog-db`): dropping it
    // and replaying every doc's frames must yield IDENTICAL rows (same op-ids,
    // ordering, content-hash sets). `rm index.db` + reopen exercises exactly
    // the on-open rebuild path.
    // status: changes-query-api
    // status: op-log-no-oplog-db
    let dir = tempdir().unwrap();
    // Build a non-trivial history across a few docs: a create + agent edit
    // accept, a rename, and a tombstone — every op-kind that writes a frame.
    {
        let log = OpLog::open(dir.path()).unwrap();
        log.create_document("a.md", "note", "hello world\n", &Author::User)
            .unwrap();
        let out = log
            .stage_pending(
                &"a.md".to_string(),
                &[EditSpec {
                    old_str: Some("world".to_string()),
                    new_str: "earth".to_string(),
                }],
                &user_ctx(),
            )
            .unwrap();
        log.accept_pending("a.md", &out.op_ids[0]).unwrap();
        log.create_document("b.md", "note", "beta\n", &Author::User)
            .unwrap();
        log.rename_document("b.md", "c.md", &Author::User).unwrap();
        log.create_document("d.md", "note", "delta\n", &Author::User)
            .unwrap();
        log.tombstone_document("d.md", &Author::User).unwrap();
    }
    // Snapshot the full vault history + per-doc content-hash sets from the
    // index as first opened.
    let snapshot = |log: &OpLog| {
        let hist = log.vault_history(1000).unwrap();
        let rows: Vec<(String, String, String, i64, Option<String>, Option<String>)> = hist
            .iter()
            .map(|m| {
                (
                    m.doc_id.clone(),
                    m.op_id.clone(),
                    m.op_kind.clone(),
                    m.timestamp_ms,
                    m.content_hash.clone(),
                    m.rename_from.clone(),
                )
            })
            .collect();
        let mut hash_sets: Vec<(String, std::collections::HashSet<String>)> = Vec::new();
        for doc in ["a.md", "c.md", "d.md"] {
            hash_sets.push((doc.to_string(), log.doc_history_hashes(doc).unwrap()));
        }
        (rows, hash_sets)
    };
    let before = {
        let log = OpLog::open(dir.path()).unwrap();
        snapshot(&log)
    };
    // Drop the regenerable index entirely (and its WAL sidecars) and reopen:
    // the on-open `rebuild_from_ops` replays the `.ops` frames.
    for ext in ["", "-wal", "-shm"] {
        let p = dir.path().join(".hiker").join(format!("index.db{ext}"));
        if p.exists() {
            std::fs::remove_file(&p).unwrap();
        }
    }
    let after = {
        let log = OpLog::open(dir.path()).unwrap();
        snapshot(&log)
    };
    assert_eq!(before.0, after.0, "history rows must be identical after rebuild");
    assert_eq!(before.1, after.1, "content-hash sets must be identical after rebuild");
    // Sanity: the rebuild actually produced rows (create+accept on a, create+
    // rename on b→c, create+tombstone on d = 6 frames).
    assert_eq!(before.0.len(), 6);
}

#[test]
fn tombstone_sets_flag_and_records_op() {
    // status: op-log-op-shape
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "hello\n", &Author::User)
        .unwrap();
    log.tombstone_document(&doc_id, &Author::User).unwrap();
    assert!(log.materialize_accepted(&doc_id).unwrap().tombstone);
    let rows = log
        .query_metadata(&meta::Filter {
            doc_id: Some(doc_id),
            ..Default::default()
        })
        .unwrap();
    assert!(rows.iter().any(|r| r.op_kind == "tombstone"));
}

#[test]
fn rename_updates_path_and_index() {
    // status: op-log-op-shape
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("old.md", "note", "hello\n", &Author::User)
        .unwrap();
    log.rename_document(&doc_id, "new.md", &Author::User).unwrap();
    // The id IS the path: the new path resolves to itself; history rows
    // repoint to the new path key (path-as-identity).
    assert_eq!(log.doc_id_for_path("new.md").unwrap(), Some("new.md".to_string()));
    let rows = log
        .query_metadata(&meta::Filter {
            doc_id: Some("new.md".to_string()),
            ..Default::default()
        })
        .unwrap();
    let rename = rows.iter().find(|r| r.op_kind == "rename").unwrap();
    assert_eq!(rename.rename_from.as_deref(), Some("old.md"));
}

#[test]
fn ops_log_is_sole_durable_representation_across_reopen() {
    // status: op-log-accepted-op-retention
    // status: op-log-materialization
    // The `.ops` history log is the document's sole durable representation now
    // that `accepted` is plain text — there is no `.yrs` snapshot. Each edit
    // appends one frame; reopening reconstructs `accepted` from the NEWEST
    // frame, so content survives a close/reopen byte-for-byte. (The
    // keyframe/delta machinery keeps the log linear; this test asserts
    // correctness, not the keyframe cadence.)
    let dir = tempdir().unwrap();
    let oplog = dir.path().join(".hiker").join("oplog");
    let ops_size = |doc_id: &str| {
        std::fs::metadata(oplog.join(format!("{doc_id}.ops"))).map(|m| m.len()).unwrap_or(0)
    };

    let doc_id;
    {
        let log = OpLog::open(dir.path()).unwrap();
        doc_id = log.create_document("a.md", "note", "seed\n", &Author::User).unwrap();
        // The create wrote the first (keyframe) frame; there is no `.yrs`.
        assert!(ops_size(&doc_id) > 0, "create writes the first .ops frame");
        assert!(
            !oplog.join(format!("{doc_id}.yrs")).exists(),
            "no .yrs snapshot is written under the text substrate",
        );
        let before = ops_size(&doc_id);
        for i in 0..40 {
            let len = log.materialize_accepted(&doc_id).unwrap().text.len();
            log.apply_user_edit(&doc_id, len, 0, &format!("line {i}\n")).unwrap();
        }
        assert!(ops_size(&doc_id) > before, "each edit appends a frame to .ops");
    }

    // Reopen: `accepted` reconstructs from the newest `.ops` frame.
    let log = OpLog::open(dir.path()).unwrap();
    let text = log.materialize_accepted(&doc_id).unwrap().text;
    assert!(text.starts_with("seed\n"), "reconstructed content starts with the seed");
    assert!(text.contains("line 39\n"), "reconstructed content includes every appended edit");

    // A further edit after reopen keeps appending and reconstructs again.
    let len = text.len();
    log.apply_user_edit(&doc_id, len, 0, "tail\n").unwrap();
    drop(log);
    let log = OpLog::open(dir.path()).unwrap();
    assert!(log.materialize_accepted(&doc_id).unwrap().text.contains("tail\n"));
}

// The op log's history query-index (`op_history`) lives in the vault's
// `index.db` and is REGENERABLE from `.ops`, so it carries no schema-version
// gate of its own — a shape change is a drop-and-replay, never a fail-loud
// migration (`op-log-no-oplog-db`). The former `version_mismatch_fails_loud`
// test guarded the now-deleted `oplog_meta.db`; the store's own `index.db`
// `user_version` fail-loud is covered by the `core::store` tests.

// ── Multi-file reorganization (reorg batch) ──────────────────────────

fn auto_cluster_ctx() -> ProducerCtx {
    ProducerCtx {
        author: Author::Auto("cluster".to_string()),
        surface: "cluster-editor".to_string(),
        session_id: None,
    }
}

#[test]
fn reorg_batch_stages_n_renames_sharing_a_batch_id() {
    // status: op-log-reorg-batch
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let a = log
        .create_document("inbox/a.md", "note", "alpha\n", &Author::User)
        .unwrap();
    let b = log
        .create_document("inbox/b.md", "note", "beta\n", &Author::User)
        .unwrap();
    let out = log
        .stage_pending_renames(
            &[
                (a.clone(), "archive/a.md".to_string()),
                (b.clone(), "archive/b.md".to_string()),
            ],
            &auto_cluster_ctx(),
        )
        .unwrap();
    assert_eq!(out.op_ids.len(), 2);
    // Both ops share the one cross-document batch_id.
    let in_batch = log.pending_ops_in_batch(&out.batch_id).unwrap();
    assert_eq!(in_batch.len(), 2);
    // Each op is a Rename authored auto:cluster, and nothing moved on disk yet.
    for doc_id in [&a, &b] {
        let pending = log.pending_ops(doc_id).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(matches!(pending[0].op_kind, OpKind::Rename { .. }));
        assert!(matches!(&pending[0].author, Author::Auto(p) if p == "cluster"));
    }
    assert!(dir.path().join("inbox/a.md").exists());
    assert!(!dir.path().join("archive/a.md").exists());
}

#[test]
fn reorg_batch_accept_moves_each_file_on_disk() {
    // status: op-log-reorg-batch
    // status: op-log-atomic-write
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let a = log
        .create_document("inbox/a.md", "note", "alpha\n", &Author::User)
        .unwrap();
    let b = log
        .create_document("inbox/b.md", "note", "beta\n", &Author::User)
        .unwrap();
    let out = log
        .stage_pending_renames(
            &[
                (a.clone(), "archive/a.md".to_string()),
                (b.clone(), "archive/b.md".to_string()),
            ],
            &auto_cluster_ctx(),
        )
        .unwrap();
    let accepted = log.accept_batch(&out.batch_id).unwrap();
    assert_eq!(accepted.len(), 2);
    // Files moved: old paths gone, new paths carry the verbatim content.
    assert!(!dir.path().join("inbox/a.md").exists());
    assert!(!dir.path().join("inbox/b.md").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("archive/a.md")).unwrap(),
        "alpha\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("archive/b.md")).unwrap(),
        "beta\n"
    );
    // The doc relabeled: old paths unresolve, new paths resolve to themselves
    // (path-as-identity), and the consumed pending queue is empty at the new id.
    assert!(log.doc_id_for_path("inbox/a.md").unwrap().is_none());
    assert_eq!(
        log.doc_id_for_path("archive/a.md").unwrap().as_deref(),
        Some("archive/a.md"),
    );
    assert!(log.pending_ops("archive/a.md").unwrap().is_empty());
    assert!(log.pending_ops("archive/b.md").unwrap().is_empty());
}

#[test]
fn reorg_batch_partial_apply_skips_a_collision() {
    // A forced collision on one move (its target already occupied by a
    // different doc) still applies the others — partial apply, not atomic.
    // status: op-log-reorg-batch
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let a = log
        .create_document("inbox/a.md", "note", "alpha\n", &Author::User)
        .unwrap();
    let b = log
        .create_document("inbox/b.md", "note", "beta\n", &Author::User)
        .unwrap();
    // `occupied.md` already exists as its own document — a's target collides.
    let _occ = log
        .create_document("archive/occupied.md", "note", "occ\n", &Author::User)
        .unwrap();
    let out = log
        .stage_pending_renames(
            &[
                (a.clone(), "archive/occupied.md".to_string()),
                (b.clone(), "archive/b.md".to_string()),
            ],
            &auto_cluster_ctx(),
        )
        .unwrap();
    let accepted = log.accept_batch(&out.batch_id).unwrap();
    // Only b's move applied; a's collided and was skipped.
    assert_eq!(accepted.len(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("archive/b.md")).unwrap(),
        "beta\n"
    );
    // a stayed put, its pending op survives for a later retry, and the
    // occupied file is untouched.
    assert!(dir.path().join("inbox/a.md").exists());
    assert_eq!(log.pending_ops(&a).unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("archive/occupied.md")).unwrap(),
        "occ\n"
    );
}

#[test]
fn reorg_batch_reject_drops_the_batch() {
    // status: op-log-reorg-batch
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let a = log
        .create_document("inbox/a.md", "note", "alpha\n", &Author::User)
        .unwrap();
    let b = log
        .create_document("inbox/b.md", "note", "beta\n", &Author::User)
        .unwrap();
    let out = log
        .stage_pending_renames(
            &[
                (a.clone(), "archive/a.md".to_string()),
                (b.clone(), "archive/b.md".to_string()),
            ],
            &auto_cluster_ctx(),
        )
        .unwrap();
    let rejected = log.reject_batch(&out.batch_id).unwrap();
    assert_eq!(rejected.len(), 2);
    // Nothing moved; queues empty; files stay at their original paths.
    assert!(dir.path().join("inbox/a.md").exists());
    assert!(dir.path().join("inbox/b.md").exists());
    assert!(log.pending_ops(&a).unwrap().is_empty());
    assert!(log.pending_ops(&b).unwrap().is_empty());
    // Rejection is transient editorial state: no durable audit rows are
    // written (`op-log-no-oplog-db`). Only the two `Create` rows remain.
    let rejected_rows = log
        .query_metadata(&meta::Filter {
            status: Some(meta::OpStatus::Rejected),
            ..Default::default()
        })
        .unwrap();
    assert!(rejected_rows.is_empty());
    let auto_rows = log
        .query_metadata(&meta::Filter {
            author_class: Some("auto".to_string()),
            ..Default::default()
        })
        .unwrap();
    assert!(auto_rows.is_empty(), "no auto-authored rows: the renames were rejected, not accepted");
}

#[test]
fn stage_pending_content_labels_frontmatter_edit() {
    // The cluster-editor tag path stages a whole new content; the op-log
    // labels it SetFrontmatter when the change lands in the fence.
    // status: op-log-reorg-batch
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "---\ntitle: A\n---\nbody\n", &Author::User)
        .unwrap();
    let out = log
        .stage_pending_content(
            &doc_id,
            "---\ntitle: A\ntags: [x]\n---\nbody\n",
            &auto_cluster_ctx(),
        )
        .unwrap();
    assert_eq!(out.op_ids.len(), 1);
    let pending = log.pending_ops(&doc_id).unwrap();
    assert!(matches!(pending[0].op_kind, OpKind::SetFrontmatter));
    // Accept lands the new content verbatim on disk.
    log.accept_pending(&doc_id, &out.op_ids[0]).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.md")).unwrap(),
        "---\ntitle: A\ntags: [x]\n---\nbody\n"
    );
}

// ── Granular (sync-correct) text edits ───────────────────────────────────

#[test]
fn multi_span_delta_localizes_and_skips_unchanged_middle() {
    // A save that touches two distant regions yields two small spans; the
    // unchanged middle appears in neither — so a concurrent remote edit over
    // it would still merge.
    // status: op-log-materialization
    let before = "alpha\nMIDDLE_UNCHANGED\nomega\n";
    let after = "ALPHA\nMIDDLE_UNCHANGED\nOMEGA\n";
    let spans = crate::merge::multi_span_delta(before, after);
    assert_eq!(spans.len(), 2, "two disjoint change regions, got {spans:?}");
    for (start, removed_len, inserted) in &spans {
        let removed = &before[*start..*start + *removed_len];
        assert!(!removed.contains("MIDDLE_UNCHANGED"), "removed touched middle");
        assert!(!inserted.contains("MIDDLE_UNCHANGED"), "inserted touched middle");
    }
    // An empty diff (no change) stages nothing.
    assert!(crate::merge::multi_span_delta(before, before).is_empty());
}

#[test]
fn user_save_commits_localized_ops_not_whole_document() {
    // The keystone sync-correctness property: a whole-buffer save diffs into
    // minimal localized TEXT spans. Flipping a few characters deep in a large
    // note produces a span covering ~that change, NOT the document length
    // (which a whole-`text` delete+reinsert would). Untouched bytes are never
    // rewritten, so a remote edit over them still merges. Under the text model
    // the proof is the diff itself (`multi_span_delta`) — the same engine the
    // commit path runs inside its lock — rather than a clock range.
    // status: op-log-materialization
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let big = format!(
        "---\ntitle: t\n---\n\n{}",
        "lorem ipsum dolor sit amet\n".repeat(60)
    );
    let doc_id = log
        .create_document("a.md", "note", &big, &Author::User)
        .unwrap();
    // Flip one word, via a full-buffer save (what the editor sends).
    let edited = big.replacen("lorem", "LOREM", 1);
    assert!(log.apply_user_text(&doc_id, &edited).unwrap());
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, edited);
    // The localized spans the commit diffed cover only the changed region —
    // orders of magnitude smaller than the >1500-byte document.
    let spans = crate::merge::multi_span_delta(&big, &edited);
    let touched: usize = spans.iter().map(|(_, removed, ins)| (*removed).max(ins.len())).sum();
    assert!(
        touched <= "LOREM".len(),
        "expected a localized edit (<= {} bytes touched), got {touched} for a {}-byte doc",
        "LOREM".len(),
        big.len()
    );
    // An unchanged re-save is a no-op: no new op recorded. History holds the
    // seed's Create op plus the single edit above = 2 rows.
    assert!(!log.apply_user_text(&doc_id, &edited).unwrap());
    assert_eq!(log.doc_history(&doc_id, 10).unwrap().len(), 2);
}

#[test]
fn accepted_row_drops_bulky_pending_text() {
    // Drift detection needs `old_str`/`new_str` only *while the op is pending*
    // (they live in `<doc-id>.pending`). Once accepted, the content is in the
    // document itself, so the durable side-table row must not keep a second
    // copy of the matched/inserted text.
    // status: op-log-op-shape
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("a.md", "note", "hello world\n", &Author::User)
        .unwrap();
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec { old_str: Some("world".to_string()), new_str: "earth".to_string() }],
            &user_ctx(),
        )
        .unwrap();
    // Pending op retains the anchor text (drift needs it).
    let pending = log.pending_ops(&doc_id).unwrap();
    assert_eq!(
        pending[0].metadata.get("old_str").and_then(|v| v.as_str()),
        Some("world")
    );
    log.accept_pending(&doc_id, &out.op_ids[0]).unwrap();
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "hello earth\n");
    // The accepted row drops the bulky text.
    let hist = log.doc_history(&doc_id, 10).unwrap();
    let row = hist.iter().find(|m| m.op_id == out.op_ids[0]).unwrap();
    assert!(row.metadata.get("old_str").is_none(), "accepted row kept old_str");
    assert!(row.metadata.get("new_str").is_none(), "accepted row kept new_str");
}

#[test]
fn unreadable_pending_queue_is_tolerated() {
    // A `.pending` left in a stale/foreign byte format (e.g. a prior on-disk
    // encoding) must not block editing: it reads as an empty queue and the
    // next stage overwrites it. Pending ops are local editorial state — an
    // unreadable queue never costs document content (which lives in `.ops`).
    // status: op-log-pending-survives-restart
    let dir = tempdir().unwrap();
    let doc_id = {
        let log = OpLog::open(dir.path()).unwrap();
        log.create_document("a.md", "note", "hello world\n", &Author::User)
            .unwrap()
    };
    // Plant non-JSON bytes where the queue file lives.
    let pending_path = dir
        .path()
        .join(".hiker")
        .join("oplog")
        .join(format!("{doc_id}.pending"));
    std::fs::write(&pending_path, [0u8, 1, 2, 3, 255]).unwrap();
    // Reopen: reading the queue must not error; it reads empty.
    let log = OpLog::open(dir.path()).unwrap();
    assert!(log.pending_ops(&doc_id).unwrap().is_empty());
    // A fresh stage works and overwrites the stale file.
    let out = log
        .stage_pending(
            &doc_id,
            &[EditSpec { old_str: Some("world".to_string()), new_str: "earth".to_string() }],
            &user_ctx(),
        )
        .unwrap();
    assert_eq!(log.pending_ops(&doc_id).unwrap().len(), 1);
    log.accept_pending(&doc_id, &out.op_ids[0]).unwrap();
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "hello earth\n");
}

#[test]
fn bug_sync_accept_pending_trusts_metadata_newpath() {
    // status: bug-sync-accept-pending-trusts-metadata-newpath
    //
    // Under the text-edit pending model a `Rename` carries exactly one source
    // of truth: `metadata["new_path"]`. `accept_pending` both repoints the path
    // index and `apply_rename`s `accepted` from that single field, so the
    // post-apply `meta.path`, the `.md` location, and the index can never
    // disagree — the desync this bug described is structurally impossible.
    //
    // We still corrupt the `.pending` JSON's `new_path` to `notes/c.md` and
    // assert the invariant the bug guarded: the on-disk `.md`, `meta.path`, and
    // the index all follow that one field in lockstep (here, `notes/c.md`).
    // Reopen so `ensure_loaded` re-reads the queue, then accept.
    let dir = tempdir().unwrap();
    let doc_id = {
        let log = OpLog::open(dir.path()).unwrap();
        let id = log
            .create_document("notes/a.md", "note", "alpha\n", &Author::User)
            .unwrap();
        let _ = log
            .stage_pending_renames(
                &[(id.clone(), "notes/b.md".to_string())],
                &auto_cluster_ctx(),
            )
            .unwrap();
        id
    };

    // Mutate the on-disk .pending: change metadata.new_path to a third path
    // (`notes/c.md`), leaving op_kind (which carries `from`) untouched. Under
    // the text model this single field now drives both the apply and the index
    // repoint, so accept must move the doc to `notes/c.md` consistently.
    let pending_path = dir
        .path()
        .join(".hiker")
        .join("oplog")
        .join(format!("{doc_id}.pending"));
    let bytes = std::fs::read(&pending_path).unwrap();
    let mut ops: Vec<super::shapes::PendingOp> =
        serde_json::from_slice(&bytes).unwrap();
    assert_eq!(ops.len(), 1);
    let op_id = ops[0].op_id.clone();
    if let serde_json::Value::Object(map) = &mut ops[0].metadata {
        map.insert(
            "new_path".to_string(),
            serde_json::Value::String("notes/c.md".to_string()),
        );
    } else {
        panic!("expected metadata object");
    }
    std::fs::write(&pending_path, serde_json::to_vec(&ops).unwrap()).unwrap();

    // Reopen so the in-memory cache is rebuilt from the mutated file.
    let log = OpLog::open(dir.path()).unwrap();
    // Sanity: the mutation round-tripped.
    let reloaded = log.pending_ops(&doc_id).unwrap();
    assert_eq!(reloaded.len(), 1);
    assert_eq!(
        reloaded[0]
            .metadata
            .get("new_path")
            .and_then(|v| v.as_str()),
        Some("notes/c.md")
    );

    log.accept_pending(&doc_id, &op_id).unwrap();

    // `apply_rename` advances meta.path to `notes/c.md` (the single source of
    // truth), so the .md lands there and the index agrees — no desync.
    assert!(
        dir.path().join("notes/c.md").exists(),
        ".md should be written at the renamed path"
    );
    assert!(
        !dir.path().join("notes/b.md").exists(),
        "the original staged path must not be written"
    );
    // The id IS the path: after the rename the doc lives at `notes/c.md`, so
    // it resolves to itself and the old id no longer resolves.
    assert_eq!(
        log.path_for_doc("notes/c.md").unwrap().as_deref(),
        Some("notes/c.md"),
        "path_for_doc must follow the applied meta.path"
    );
    assert_eq!(log.path_for_doc(&doc_id).unwrap(), None);
    assert_eq!(
        log.doc_id_for_path("notes/c.md").unwrap().as_deref(),
        Some("notes/c.md"),
        "doc_id_for_path on the renamed path must resolve"
    );
    assert_eq!(
        log.doc_id_for_path("notes/b.md").unwrap(),
        None,
        "the original staged path must NOT be in the index"
    );
}


#[test]
fn reextract_replace_applies_extractor_op_and_replaces_body() {
    // status: op-log-reextract-replace
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("clips/a.md", "sidecar", FRONTMATTER_DOC, &Author::User)
        .unwrap();
    let before = log.doc_history(&doc_id, 100).unwrap().len();

    // A changed re-extraction lands one new version, authored extractor:<id>.
    let new_body = "\n# Heading\n\nUpdated extracted text.\n";
    let landed = log.reextract_replace(&doc_id, new_body, "web").unwrap();
    assert!(landed, "a changed re-extraction must land a version");

    let after = log.materialize_accepted(&doc_id).unwrap().text;
    // Frontmatter fence preserved verbatim; only the body is the new extraction.
    let fence_end = super::shapes::frontmatter_fence_end(FRONTMATTER_DOC).unwrap();
    assert_eq!(&after[..fence_end], &FRONTMATTER_DOC[..fence_end]);
    assert_eq!(&after[fence_end..], new_body);

    // History grew by exactly one, and the newest op is extractor-authored.
    let hist = log.doc_history(&doc_id, 100).unwrap();
    assert_eq!(hist.len(), before + 1);
    assert_eq!(hist[0].author, Author::Extractor("web".to_string()));

    // Disk == materialize(accepted) (the canonical invariant).
    let on_disk = std::fs::read_to_string(dir.path().join("clips/a.md")).unwrap();
    assert_eq!(on_disk, after);
}

#[test]
fn reextract_replace_identical_is_a_noop() {
    // status: op-log-reextract-replace
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("clips/a.md", "sidecar", FRONTMATTER_DOC, &Author::User)
        .unwrap();
    let before = log.doc_history(&doc_id, 100).unwrap().len();

    // Re-extracting the *same* body produces no op, no version.
    let fence_end = super::shapes::frontmatter_fence_end(FRONTMATTER_DOC).unwrap();
    let same_body = &FRONTMATTER_DOC[fence_end..];
    let landed = log.reextract_replace(&doc_id, same_body, "web").unwrap();
    assert!(!landed, "an identical re-extraction must be a no-op");
    assert_eq!(log.doc_history(&doc_id, 100).unwrap().len(), before);
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, FRONTMATTER_DOC);
}

#[test]
fn reextract_replace_leaves_frontmatter_when_no_fence() {
    // status: op-log-reextract-replace
    // A doc with no frontmatter fence: the whole text is body.
    let dir = tempdir().unwrap();
    let log = OpLog::open(dir.path()).unwrap();
    let doc_id = log
        .create_document("clips/b.md", "sidecar", "old body\n", &Author::User)
        .unwrap();
    let landed = log.reextract_replace(&doc_id, "new body\n", "pdf").unwrap();
    assert!(landed);
    assert_eq!(log.materialize_accepted(&doc_id).unwrap().text, "new body\n");
}
