//! Integration coverage for the canvas file-ref rename rewrite. Builds a real
//! vault with one or more `.canvas` documents, runs the sweep, and asserts the
//! File-node `file` paths follow the move while non-matching nodes and
//! unparseable canvases are left alone.
//!
//! status: canvas-file-ref-rewrite

use std::sync::Arc;

use tempfile::TempDir;

use crate::editing::LayeredDoc;
use crate::vault::Vault;

const CANVAS: &str = r##"{
	"nodes": [
		{
			"id": "n1",
			"x": 0,
			"y": 0,
			"width": 200,
			"height": 120,
			"type": "file",
			"file": "old/path.md",
			"subpath": "#intro"
		},
		{
			"id": "n2",
			"x": 400,
			"y": 0,
			"width": 200,
			"height": 120,
			"type": "file",
			"file": "other/keep.md"
		}
	],
	"edges": []
}
"##;

fn write(vault: &Vault, rel: &str, body: &str) {
    let abs = vault.abs_path(rel).unwrap();
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(abs, body).unwrap();
}

#[tokio::test]
async fn canvas_file_ref_follows_note_move_no_log() {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    write(&vault, "diagrams/board.canvas", CANVAS);

    let touched =
        super::on_note_moved(None, None, None, &vault, "old/path.md", "new/path.md").await;
    assert_eq!(touched, 1, "the one referencing canvas should be rewritten");

    let after = vault.read_file("diagrams/board.canvas").unwrap();
    assert!(after.contains("\"file\": \"new/path.md\""), "matched ref must rewrite");
    assert!(after.contains("\"subpath\": \"#intro\""), "subpath must survive");
    assert!(after.contains("\"file\": \"other/keep.md\""), "non-matching ref untouched");
    assert!(!after.contains("old/path.md"), "old path must be gone");
}

#[tokio::test]
async fn canvas_file_ref_follows_note_move_through_layered_doc() {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let log = Arc::new(LayeredDoc::open(td.path()).unwrap());
    write(&vault, "diagrams/board.canvas", CANVAS);

    let touched = super::on_note_moved(
        None,
        None,
        Some(&log),
        &vault,
        "old/path.md",
        "new/path.md",
    )
    .await;
    assert_eq!(touched, 1, "the referencing canvas should be rewritten via op-log");

    // The layered doc writes the new bytes to disk on user_save, so the file
    // reflects the rewrite without a separate write.
    let after = vault.read_file("diagrams/board.canvas").unwrap();
    assert!(after.contains("\"file\": \"new/path.md\""), "op-log save must persist rewrite");
    assert!(!after.contains("old/path.md"), "old path must be gone after op-log save");
}

#[tokio::test]
async fn unparseable_canvas_is_skipped() {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    write(&vault, "broken.canvas", "{ this is not valid JSON canvas");
    write(&vault, "good.canvas", CANVAS);

    let touched =
        super::on_note_moved(None, None, None, &vault, "old/path.md", "new/path.md").await;
    // Only the good canvas is rewritten; the broken one is skipped, not fatal.
    assert_eq!(touched, 1, "broken canvas must be skipped, good one rewritten");
    let broken = vault.read_file("broken.canvas").unwrap();
    assert_eq!(broken, "{ this is not valid JSON canvas", "broken canvas left untouched");
}

#[tokio::test]
async fn no_referrers_is_a_noop() {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    write(&vault, "board.canvas", CANVAS);

    let touched =
        super::on_note_moved(None, None, None, &vault, "absent/note.md", "new/note.md").await;
    assert_eq!(touched, 0, "no canvas references the moved note");
    assert_eq!(vault.read_file("board.canvas").unwrap(), CANVAS, "canvas byte-identical");
}

#[test]
fn canvases_referencing_returns_only_canvases_with_a_matching_file_node() {
    // status: canvas-appears-in
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    // One canvas references old/path.md (+ other/keep.md); a second references
    // only an unrelated note; a third is unparseable and must be skipped, not
    // abort the scan.
    write(&vault, "diagrams/board.canvas", CANVAS);
    write(
        &vault,
        "diagrams/other.canvas",
        r##"{"nodes":[{"id":"a","x":0,"y":0,"width":10,"height":10,"type":"file","file":"unrelated/note.md"}],"edges":[]}"##,
    );
    write(&vault, "diagrams/broken.canvas", "{ not valid json");

    let hits = super::canvases_referencing(&vault, "old/path.md").unwrap();
    assert_eq!(hits, vec!["diagrams/board.canvas".to_string()]);

    // A second File node in the same canvas is matched independently.
    let keep = super::canvases_referencing(&vault, "other/keep.md").unwrap();
    assert_eq!(keep, vec!["diagrams/board.canvas".to_string()]);

    // A note no canvas references yields nothing.
    assert!(
        super::canvases_referencing(&vault, "nobody/refs.md")
            .unwrap()
            .is_empty()
    );
}
