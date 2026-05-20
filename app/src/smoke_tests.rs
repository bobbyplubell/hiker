//! Smoke tests via `egui_kittest`. Drive the app for a few frames with a
//! tempdir vault and assert no panic. These exist to catch regressions
//! like the recent egui_dock `node was not a leaf` panic that fired at
//! startup — anything that survives a few frames of paint is unlikely
//! to crash the user on launch.
//!
//! Tests live inside the binary crate (gated by `#[cfg(test)]`) so they
//! can reach private modules without forcing a lib.rs split. Heavy
//! services (MCP, full index scan, LLM worker) are exercised through
//! the real `bootstrap::open_vault` path — that's the same code path
//! the user hits at startup, and any panic surface there is worth
//! catching.
//!
//! See `scripts/check.sh` — `cargo test -p hiker-app --tests` runs
//! every `#[test]` in this module.

use std::sync::Arc;

use eframe::egui;

use crate::bootstrap;
use crate::state::AppState;
use crate::tabs;

/// Build a tokio runtime + open a tempdir-backed vault. Returns the
/// runtime as an `Arc` (the app keeps it that way) and a constructed
/// `AppState`. Disable MCP and the LLM worker via the config so the
/// test doesn't bind a real port or try to talk to a network endpoint.
fn open_temp_vault() -> (Arc<tokio::runtime::Runtime>, AppState) {
    let tmpdir = tempfile::tempdir().expect("create tempdir");
    let vault_root = tmpdir.path().to_path_buf();
    // Write a minimal hiker config that disables MCP + LLM. The path is
    // <vault>/.hiker/config.toml per `hiker_core::config::Config::load`.
    let hiker_dir = vault_root.join(".hiker");
    std::fs::create_dir_all(&hiker_dir).unwrap();
    let config_toml = r#"
[mcp]
enabled = false

[llm]
enabled = false
"#;
    std::fs::write(hiker_dir.join("config.toml"), config_toml).unwrap();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let state = runtime
        .block_on(async { bootstrap::open_vault(vault_root).await })
        .expect("open vault");
    // Keep the tempdir alive for the duration of the test by leaking it.
    // The test runs to completion in seconds; the dir gets cleaned up by
    // the OS afterwards. Holding it across the move into the closure is
    // awkward otherwise.
    std::mem::forget(tmpdir);
    (Arc::new(runtime), state)
}

/// Drive `tabs::body` (the egui_dock central area) for a few frames in
/// a headless kittest harness. This catches the kind of "second-frame
/// panic" that bit us when the stray-tab enforcement tried to
/// `move_tab` into a `Node::Empty` destination.
#[test]
fn dock_body_runs_clean_for_three_frames() {
    let (runtime, mut state) = open_temp_vault();
    let _guard = runtime.enter();

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1400.0, 900.0))
        .build(|ctx: &egui::Context| {
            tabs::body(ctx, &mut state, &runtime);
        });

    // Three frames is enough to exercise: (1) initial reconcile that
    // adds the auto-Home tab, (2) the steady-state pass, (3) any
    // reactions to layout from frame 2. If any of these panic, the
    // test fails loudly.
    for _ in 0..3 {
        harness.run();
    }
}

/// `default_dock` must produce a center_tile that resolves to a Tabs
/// container — the reconciler relies on this for placing new buffer
/// tabs.
#[test]
fn default_dock_has_center_tile() {
    use egui_tiles::{Container, Tile};
    let bundle = crate::layout::default_dock();
    let tile = bundle.tree.tiles.get(bundle.center_tile);
    assert!(
        matches!(tile, Some(Tile::Container(Container::Tabs(_)))),
        "default_dock center_tile must be a Tabs container",
    );
}

/// Buffer tabs that wander into a side panel container must be moved
/// back to the center tile by `enforce_buffer_tabs_in_center`.
#[test]
fn enforce_buffer_tabs_in_center_moves_stray() {
    use crate::tab::{DockTab, TabId};
    use egui_tiles::{Container, Tile};
    let mut bundle = crate::layout::default_dock();
    // Insert a buffer tab pane into the LEFT tile.
    let stray = bundle.tree.tiles.insert_pane(DockTab::Tab(TabId(42)));
    if let Some(Tile::Container(Container::Tabs(tabs))) =
        bundle.tree.tiles.get_mut(bundle.left_tile)
    {
        tabs.add_child(stray);
    }
    crate::layout::enforce_buffer_tabs_in_center(&mut bundle.tree, bundle.center_tile);
    let parent = bundle.tree.tiles.parent_of(stray);
    assert_eq!(
        parent,
        Some(bundle.center_tile),
        "stray buffer tab should be moved back to center_tile",
    );
}
