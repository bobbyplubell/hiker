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

/// Drive `tabs::body` (the egui_dock central area) for a few frames in
/// a headless kittest harness. This catches the kind of "second-frame
/// panic" that bit us when the stray-tab enforcement tried to
/// `move_tab` into a `Node::Empty` destination.
/// Build a tokio runtime + open a tempdir-backed vault for a smoke test.
/// MCP + the LLM worker are disabled via config so tests don't bind a real
/// port or hit the network. The tempdir is leaked (OS reclaims it) so the
/// returned state isn't tied to a guard the caller has to thread through.
fn open_temp_vault() -> (crate::state::AppState, Arc<tokio::runtime::Runtime>) {
    let tmpdir = tempfile::tempdir().expect("create tempdir");
    let vault_root = tmpdir.path().to_path_buf();
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
    std::mem::forget(tmpdir);
    (state, Arc::new(runtime))
}

#[test]
fn dock_body_runs_clean_for_three_frames() {
    let (mut state, runtime) = open_temp_vault();
    let _guard = runtime.enter();

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1400.0, 900.0))
        .build(|ctx: &egui::Context| {
            state.dock_body(ctx, &runtime);
        });

    // Three frames is enough to exercise: (1) initial reconcile that
    // adds the auto-Home tab, (2) the steady-state pass, (3) any
    // reactions to layout from frame 2. If any of these panic, the
    // test fails loudly.
    for _ in 0..3 {
        harness.run();
    }
}

/// Render a tool-call card through the structured path — JSON args with a
/// markdown `content` field (which embeds a read-only editor preview) plus
/// a JSON result — for a few frames. Guards the embed's first-frame /
/// settled-height handoff and the JSON-shaping helpers against panics.
#[test]
fn tool_card_structured_render_runs_clean() {
    use crate::chat::state::{ChatRole, ChatSession, ChatTurn, ToolCard};

    let (mut state, runtime) = open_temp_vault();
    let _guard = runtime.enter();

    let session_id = "smoke".to_string();
    let session = ChatSession {
        id: session_id.clone(),
        turns: vec![ChatTurn {
            role: ChatRole::Tool,
            text: String::new(),
            tool: Some(ToolCard {
                tool_name: "write_note".to_string(),
                args: r##"{"path":"a/b.md","content":"# Title\n\nSome **bold** text.\n\n- one\n- two\n"}"##
                    .to_string(),
                result: Some(r#"{"status":"written","path":"a/b.md"}"#.to_string()),
                ok: true,
                produced_write: true,
                target_path: Some("a/b.md".to_string()),
            }),
        }],
        ..ChatSession::default()
    };
    state.session.chat.sessions.insert(session_id.clone(), session);
    state.session.chat.active = Some(session_id.clone());

    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(1400.0, 900.0))
        .build(|ctx: &egui::Context| {
            egui::CentralPanel::default().show(ctx, |ui| {
                crate::chat::render::show(
                    ui,
                    &mut state,
                    Some(&session_id),
                    crate::chat::render::Layout::FullTab,
                    &runtime,
                );
            });
        });

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
    {
        use crate::layout::DockTreeExt as _;
        bundle.tree.enforce_buffer_tabs_in_center(bundle.center_tile);
    }
    let parent = bundle.tree.tiles.parent_of(stray);
    assert_eq!(
        parent,
        Some(bundle.center_tile),
        "stray buffer tab should be moved back to center_tile",
    );
}

/// Build a real `AppState` over a tempdir vault containing a single note, with
/// MCP + LLM disabled. Leaks the tempdir (the OS reaps it) and returns the
/// runtime so its background services stay alive for the test's lifetime.
#[cfg(test)]
fn open_vault_with_note(name: &str, content: &str) -> (crate::state::AppState, Arc<tokio::runtime::Runtime>) {
    let tmpdir = tempfile::tempdir().expect("create tempdir");
    let root = tmpdir.path().to_path_buf();
    let hiker_dir = root.join(".hiker");
    std::fs::create_dir_all(&hiker_dir).unwrap();
    std::fs::write(hiker_dir.join("config.toml"), "[mcp]\nenabled = false\n[llm]\nenabled = false\n").unwrap();
    std::fs::write(root.join(name), content).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let state = runtime.block_on(async { bootstrap::open_vault(root).await }).expect("open vault");
    std::mem::forget(tmpdir);
    (state, Arc::new(runtime))
}

/// Whether the active tab is a read-only preview (snapshot/proposal) of `path`.
/// (`vault_path()` is `None` for non-`Vault` sources, so match the buffer
/// source's own path + a diff layer.)
#[cfg(test)]
fn active_is_preview_of(state: &crate::state::AppState, path: &str) -> bool {
    use crate::tab::{BufferSource, TabKind};
    state.session.active_tab.and_then(|id| state.tab_by_id(id)).is_some_and(|t| {
        matches!(
            &t.kind,
            TabKind::Editor { buffer, diff: Some(_) }
                if buffer.path() == path && !matches!(buffer, BufferSource::Vault { .. })
        )
    })
}

/// Whether the active tab is the live vault buffer for `path`.
#[cfg(test)]
fn active_is_live(state: &crate::state::AppState, path: &str) -> bool {
    state.session.active_tab.and_then(|id| state.tab_by_id(id)).is_some_and(|t| {
        t.kind.vault_path() == Some(path) && t.kind.diff_source().is_none()
    })
}

#[test]
fn snapshot_open_and_back_round_trips_in_the_active_tab() {
    use crate::editor_pane;
    use crate::state::NavTarget;
    let (mut state, _rt) = open_vault_with_note("note.md", "version one\n");
    let log = state.vault_session.services.oplog.clone();
    let vault = state.vault_session.vault.clone();
    // A second accepted version, so there's a prior op to snapshot to.
    hiker_core::ops::op_writes::user_save(&log, &vault, "note.md", "version two\n").unwrap();
    let history = hiker_core::ops::op_writes::path_history(&log, "note.md", 10).unwrap();
    let op_id = history.first().expect("an accepted op").op_id.clone();

    // Open the live note in a tab.
    editor_pane::open_file(&mut state, "note.md", /* sticky */ true);
    assert!(active_is_live(&state, "note.md"), "live note open in the active tab");
    assert_eq!(state.session.nav.current(), Some(&NavTarget::File("note.md".to_string())));
    let tab_count = state.session.tabs.len();

    // Pick a snapshot from the dropdown → swaps the ACTIVE tab in place.
    editor_pane::open_snapshot_in_tab(&mut state, "note.md", &op_id);
    assert_eq!(state.session.tabs.len(), tab_count, "snapshot must NOT open a new tab");
    assert!(active_is_preview_of(&state, "note.md"), "active tab now shows the snapshot");
    assert_eq!(
        state.session.nav.current(),
        Some(&NavTarget::Snapshot { path: "note.md".to_string(), op_id: op_id.clone() }),
    );
    // And the snapshot content actually loads (no "couldn't load the buffer").
    let src = crate::tab::BufferSource::Snapshot { path: "note.md".to_string(), op_id: op_id.clone() };
    assert!(
        editor_pane::ensure_readonly_buffer_loaded(&mut state, &src).is_some(),
        "snapshot buffer materializes",
    );

    // Back → returns to the live note IN THE SAME tab (no new tab).
    editor_pane::nav_go(&mut state, -1);
    assert_eq!(state.session.tabs.len(), tab_count, "Back must not spawn a tab");
    assert!(active_is_live(&state, "note.md"), "Back reverts the active tab to the live note");
    assert_eq!(state.session.nav.current(), Some(&NavTarget::File("note.md".to_string())));

    // Forward → re-shows the snapshot in place.
    editor_pane::nav_go(&mut state, 1);
    assert!(active_is_preview_of(&state, "note.md"), "Forward re-shows the snapshot");
}

#[test]
fn back_forward_between_two_files_still_works() {
    // Regression guard: the nav refactor must not break plain file↔file
    // back/forward.
    use crate::editor_pane;
    use crate::state::NavTarget;
    let (mut state, _rt) = open_vault_with_note("a.md", "alpha\n");
    std::fs::write(state.vault_session.vault_root.join("b.md"), "bravo\n").unwrap();

    editor_pane::open_file(&mut state, "a.md", true);
    editor_pane::open_file(&mut state, "b.md", true);
    assert!(active_is_live(&state, "b.md"));
    assert_eq!(state.session.nav.current(), Some(&NavTarget::File("b.md".to_string())));

    editor_pane::nav_go(&mut state, -1);
    assert!(active_is_live(&state, "a.md"), "Back focuses the earlier file");
    editor_pane::nav_go(&mut state, 1);
    assert!(active_is_live(&state, "b.md"), "Forward returns to the later file");
}

#[test]
fn returning_to_live_from_a_snapshot_loads_the_buffer() {
    // The snapshot may have been opened as a fresh tab (Home / Changes) with no
    // live buffer cached. Picking "Live" from the version dropdown (or Back)
    // must (re)load it, not leave a blank "Couldn't load the buffer" tab.
    use crate::editor_pane;
    let (mut state, _rt) = open_vault_with_note("note.md", "version one\n");
    let log = state.vault_session.services.oplog.clone();
    let vault = state.vault_session.vault.clone();
    hiker_core::ops::op_writes::user_save(&log, &vault, "note.md", "version two\n").unwrap();
    let op_id = hiker_core::ops::op_writes::path_history(&log, "note.md", 10)
        .unwrap()
        .first()
        .unwrap()
        .op_id
        .clone();

    editor_pane::open_file(&mut state, "note.md", true);
    editor_pane::open_snapshot_in_tab(&mut state, "note.md", &op_id);
    assert!(active_is_preview_of(&state, "note.md"));
    // Drop the cached live buffer to mimic a snapshot opened in a fresh tab.
    state.session.buffers.remove("note.md");

    editor_pane::open_live_in_tab(&mut state, "note.md");
    assert!(active_is_live(&state, "note.md"), "active tab back on the live buffer");
    assert!(
        state.session.buffers.contains_key("note.md"),
        "the live buffer was (re)loaded, not left blank",
    );
}
