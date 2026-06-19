//! End-to-end test against a real rust-analyzer process. Gated: runs ONLY when `HIKER_LSP_E2E=1`
//! AND rust-analyzer is present. It is also `#[ignore]`d so a bare `cargo test` never spawns RA.
//!
//! Run it explicitly with:
//!   HIKER_LSP_E2E=1 cargo test -p hiker-lsp --test e2e -- --ignored --nocapture
//!
//! It spawns RA on the `hiker-code` crate (a small, fast-indexing real Rust project inside this
//! sub-workspace), resolves `ScipAdapter`, asserts the hit lands in `scip_adapter.rs` (and prefers
//! the struct DEFINITION over the `pub use` re-export), then — because a *struct* has no call
//! hierarchy — resolves the `entity_kind` **function** and asserts its call-hierarchy neighbors are
//! non-empty (set membership, not order).

use std::path::{Path, PathBuf};

use hiker_lsp::LspAdapter;
use spec_engine::{DerivedNodeSource, EdgeKind, SourceId};

fn ra_program() -> Option<PathBuf> {
    let local = PathBuf::from(std::env::var("HOME").ok()?).join(".local/bin/rust-analyzer");
    if local.exists() {
        return Some(local);
    }
    // fall back to PATH lookup via `which`-style: trust the bare name if discoverable.
    Some(PathBuf::from("rust-analyzer"))
}

#[test]
#[ignore = "spawns rust-analyzer; gated on HIKER_LSP_E2E=1"]
fn resolves_scip_adapter_and_has_call_neighbors() {
    if std::env::var("HIKER_LSP_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set HIKER_LSP_E2E=1 to run");
        return;
    }
    let Some(program) = ra_program() else {
        eprintln!("skipping: rust-analyzer not found");
        return;
    };
    // hiker-code lives at ../hiker-code relative to this crate's manifest dir.
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../hiker-code");
    let source = SourceId("lsp".to_string());

    let adapter = LspAdapter::spawn(&program, &repo_root, "ScipAdapter", source.clone())
        .expect("rust-analyzer should become ready within the budget");

    // resolve lands on the struct definition in scip_adapter.rs (not the lib.rs re-export).
    let handle = adapter.resolve("ScipAdapter", &source).expect("resolve ScipAdapter");
    let loc = adapter.locate(&handle).expect("locate resolved handle");
    assert!(
        loc.file.contains("scip_adapter.rs"),
        "expected hit in scip_adapter.rs, got {}",
        loc.file
    );

    // Call hierarchy: a struct has none, so prove blast-radius on a function.
    let func = adapter.resolve("entity_kind", &source).expect("resolve entity_kind");
    let floc = adapter.locate(&func).expect("locate entity_kind");
    assert!(floc.file.contains("scip_adapter.rs"), "entity_kind in scip_adapter.rs");
    let neighbors = adapter.neighbors(&func, &[EdgeKind::Calls]);
    assert!(!neighbors.is_empty(), "expected non-empty call neighbors for entity_kind");
}
