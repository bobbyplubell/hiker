//! Tests for the one-time storage-layout migration that relocates legacy
//! hidden `.hiker/trails/<id>/waypoints/` dirs to the trail-doc's visible
//! companion folder (`trail-storage-layout`, `note-companion-folder`).

use std::sync::Arc;

use crate::oplog::shapes::Author;
use crate::oplog::OpLog;
use crate::trails::ops::migrate_waypoints_to_companion_folders;
use crate::vault::Vault;

use tempfile::TempDir;

fn setup() -> (TempDir, Vault, Arc<OpLog>) {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let log = Arc::new(OpLog::open(td.path()).unwrap());
    (td, vault, log)
}

// status: trail-storage-layout / note-companion-folder
#[test]
fn migration_relocates_waypoints_and_rewrites_trail_doc() {
    let (td, vault, log) = setup();

    // A trail-doc at the visible location, seeded in the op-log so the
    // migration can resolve its path from its doc_id.
    std::fs::create_dir_all(td.path().join("trails")).unwrap();
    // Trail-doc frontmatter still points at the OLD hidden waypoint paths.
    // We don't know the doc_id yet; write a placeholder then rewrite once
    // we know it — simpler: seed first to learn the id, then write the doc.
    let trail_id = log
        .create_document("trails/t.md", "markdown", "seed", &Author::User)
        .unwrap();

    let trail_doc = format!(
        "---\nhiker:\n  kind: trail\n  waypoints:\n    - path: .hiker/trails/{trail_id}/waypoints/a--AAAAAA.md\n    - path: .hiker/trails/{trail_id}/waypoints/b--BBBBBB.md\n---\nbody\n"
    );
    std::fs::write(td.path().join("trails/t.md"), &trail_doc).unwrap();

    // Two legacy hidden waypoint-notes.
    let wp_dir = td
        .path()
        .join(format!(".hiker/trails/{trail_id}/waypoints"));
    std::fs::create_dir_all(&wp_dir).unwrap();
    let wp = |src: &str| {
        format!(
            "---\nhiker:\n  kind: waypoint\n  references:\n    path: {src}\n  in_trail:\n    path: trails/t.md\n---\n"
        )
    };
    std::fs::write(wp_dir.join("a--AAAAAA.md"), wp("research/a.md")).unwrap();
    std::fs::write(wp_dir.join("b--BBBBBB.md"), wp("research/b.md")).unwrap();
    // Seed the waypoints in the op-log at their hidden paths so the
    // migration's path-mapping repoint has something to update.
    let wp_a_old = format!(".hiker/trails/{trail_id}/waypoints/a--AAAAAA.md");
    let wp_b_old = format!(".hiker/trails/{trail_id}/waypoints/b--BBBBBB.md");
    log.create_document(&wp_a_old, "markdown", "", &Author::User)
        .unwrap();
    log.create_document(&wp_b_old, "markdown", "", &Author::User)
        .unwrap();

    let migrated = migrate_waypoints_to_companion_folders(&vault, &log).unwrap();
    assert_eq!(migrated, 1);

    // Waypoints moved to the visible companion folder.
    assert!(td.path().join("trails/t/a--AAAAAA.md").exists());
    assert!(td.path().join("trails/t/b--BBBBBB.md").exists());
    // The legacy hidden dir is gone.
    assert!(!td.path().join(format!(".hiker/trails/{trail_id}")).exists());

    // The trail-doc's `hiker.waypoints[].path` entries were rewritten.
    let rewritten = std::fs::read_to_string(td.path().join("trails/t.md")).unwrap();
    let fm = crate::trails::parse_trail_doc(&rewritten).unwrap();
    assert_eq!(fm.waypoints[0].path, "trails/t/a--AAAAAA.md");
    assert_eq!(fm.waypoints[1].path, "trails/t/b--BBBBBB.md");

    // The op-log path mappings repointed to the new visible paths.
    assert!(log
        .doc_id_for_path("trails/t/a--AAAAAA.md")
        .unwrap()
        .is_some());
    assert!(log.doc_id_for_path(&wp_a_old).unwrap().is_none());

    // Idempotent: a second run moves nothing.
    let again = migrate_waypoints_to_companion_folders(&vault, &log).unwrap();
    assert_eq!(again, 0);
}

// status: trail-storage-layout / note-companion-folder
#[test]
fn migration_skips_drafts_dir() {
    let (td, vault, log) = setup();

    // A draft trail-doc + its hidden companion folder under
    // `.hiker/trails/drafts/`. The migration must leave both in place.
    let drafts = td.path().join(".hiker/trails/drafts");
    std::fs::create_dir_all(drafts.join("01DRAFT")).unwrap();
    std::fs::write(
        drafts.join("01DRAFT.md"),
        "---\nhiker:\n  kind: trail\n  draft: true\n---\n",
    )
    .unwrap();
    std::fs::write(drafts.join("01DRAFT/wp--AAAAAA.md"), "x").unwrap();

    let migrated = migrate_waypoints_to_companion_folders(&vault, &log).unwrap();
    assert_eq!(migrated, 0);
    // Draft + its companion folder untouched.
    assert!(td.path().join(".hiker/trails/drafts/01DRAFT.md").exists());
    assert!(td
        .path()
        .join(".hiker/trails/drafts/01DRAFT/wp--AAAAAA.md")
        .exists());
}

// status: trail-storage-layout / note-companion-folder
#[test]
fn migration_no_op_on_fresh_vault() {
    let (_td, vault, log) = setup();
    let migrated = migrate_waypoints_to_companion_folders(&vault, &log).unwrap();
    assert_eq!(migrated, 0);
}
