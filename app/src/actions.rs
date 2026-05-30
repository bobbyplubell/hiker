//! Action registry — single source of truth for "things the user can do
//! from a toolbar button, a menu row, or the command palette".
//!
//! Each `Action` carries metadata (label, icon, category) and a `run`
//! function that mutates `AppState`. The registry is a static `LazyLock`
//! collected once at process start; lookups are O(1) by id via a HashMap.
//!
//! Step 3 of the egui_dock migration adds this layer so the top toolbar
//! can be data-driven (a `Vec<ActionId>` per toolbar) and so a Ctrl+K
//! palette can list/run every action. Existing keybinds still dispatch
//! directly; migrating them through `dispatch()` is a later step.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::editor_pane;
use crate::icons;
use crate::state::{AppState, ToastLevel, nav_can_back, nav_can_forward};
use crate::tab::TabKind;

pub type ActionId = &'static str;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionCategory {
    Navigation,
    View,
    Vault,
    File,
    Editor,
    Tab,
    Chat,
    Palette,
    Panel,
}

impl ActionCategory {
    pub const fn label(self) -> &'static str {
        match self {
            ActionCategory::Navigation => "Navigation",
            ActionCategory::View => "View",
            ActionCategory::Vault => "Vault",
            ActionCategory::File => "File",
            ActionCategory::Editor => "Editor",
            ActionCategory::Tab => "Tab",
            ActionCategory::Chat => "Chat",
            ActionCategory::Palette => "Palette",
            ActionCategory::Panel => "Panel",
        }
    }
}

pub struct Action {
    pub id: ActionId,
    pub icon: icons::Icon,
    pub label: &'static str,
    pub badge: Option<fn(&AppState) -> Option<String>>,
    pub enabled: Option<fn(&AppState) -> bool>,
    pub run: fn(&mut AppState),
    pub category: ActionCategory,
}

pub struct ActionRegistry {
    actions: Vec<&'static Action>,
    by_id: HashMap<ActionId, &'static Action>,
}

impl ActionRegistry {
    pub fn all() -> &'static ActionRegistry {
        &REGISTRY
    }

    pub fn by_id(&self, id: &str) -> Option<&'static Action> {
        self.by_id.get(id).copied()
    }

    pub fn list(&self) -> &[&'static Action] {
        &self.actions
    }
}

/// Look up and invoke an action by id. Honors the `enabled` predicate;
/// silently no-ops if the id is unknown or the action is disabled.
pub fn dispatch(app: &mut AppState, id: &str) {
    let Some(action) = ActionRegistry::all().by_id(id) else {
        return;
    };
    if let Some(en) = action.enabled
        && !en(app)
    {
        return;
    }
    (action.run)(app);
}

// ---- Synthetic / non-action ids used by toolbar layout ------------------

/// Vertical separator. Not a registered action; recognised by the toolbar
/// renderer and rendered as `ui.separator()`.
pub const ID_SEP: &str = "sep";

/// Flexible space that pushes subsequent ids to the far edge of the
/// toolbar. Not a registered action; recognised by the toolbar renderer.
pub const ID_SPACER: &str = "spacer";

/// Composite "more actions" hamburger. Not a registered action — the
/// renderer special-cases this id to draw the existing dropdown.
pub const ID_ACTIONS_MENU: &str = "actions.menu";

/// Vault-name label with context menu. Not a registered action — the
/// renderer special-cases this id to draw the existing label.
pub const ID_VAULT_LABEL: &str = "vault.label";

#[allow(dead_code)]
pub fn is_layout_id(id: &str) -> bool {
    matches!(id, ID_SEP | ID_SPACER | ID_ACTIONS_MENU | ID_VAULT_LABEL)
}

// ---- Action definitions -------------------------------------------------

fn open_singleton(state: &mut AppState, kind: TabKind) {
    crate::toolbar::open_singleton_tab(state, kind);
}

static A_NAV_BACK: Action = Action {
    id: "nav.back",
    icon: icons::Icon::Back,
    label: "Back",
    badge: None,
    enabled: Some(nav_can_back),
    run: |s| editor_pane::nav_go(s, -1),
    category: ActionCategory::Navigation,
};

static A_NAV_FORWARD: Action = Action {
    id: "nav.forward",
    icon: icons::Icon::Forward,
    label: "Forward",
    badge: None,
    enabled: Some(nav_can_forward),
    run: |s| editor_pane::nav_go(s, 1),
    category: ActionCategory::Navigation,
};

static A_NAV_HOME: Action = Action {
    id: "nav.home",
    icon: icons::Icon::Home,
    label: "Home",
    badge: None,
    enabled: None,
    run: |s| open_singleton(s, TabKind::Home),
    category: ActionCategory::Navigation,
};

static A_VAULT_SWITCH: Action = Action {
    id: "vault.switch",
    icon: icons::Icon::Folder,
    label: "Open vault",
    badge: None,
    enabled: None,
    run: |state| {
        // Drive the folder picker on the tokio runtime rather than calling
        // the synchronous `rfd::FileDialog` here: that blocks the egui/winit
        // thread for the dialog's whole lifetime (plus the portal round-trip
        // to even show it), which freezes every repaint. We hand the choice
        // back through a oneshot that `progress_vault_switch` polls each
        // frame — see `VaultSwitchState::Picking`.
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<std::path::PathBuf>>();
        let dir = state.vault_session.vault_root.clone();
        tokio::spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Pick a vault folder")
                .set_directory(&dir)
                .pick_folder()
                .await
                .map(|h| h.path().to_path_buf());
            // Receiver dropped (a newer request superseded us) → discard.
            let _ = tx.send(picked);
        });
        state.vault_switch = crate::state::VaultSwitchState::Picking(rx);
    },
    category: ActionCategory::Vault,
};

static A_VAULT_SETTINGS: Action = Action {
    id: "vault.open_settings",
    icon: icons::Icon::Settings,
    label: "Settings",
    badge: None,
    enabled: None,
    run: |s| open_singleton(s, TabKind::Settings),
    category: ActionCategory::Vault,
};

static A_VAULT_QUEUE: Action = Action {
    id: "vault.open_queue",
    icon: icons::Icon::Clipboard,
    label: "Queue",
    badge: Some(|state| {
        use hiker_core::tasks::types::TaskState;
        let n = state
            .ui_cache
            .task_snapshot
            .iter()
            .filter(|r| matches!(r.state, TaskState::Queued | TaskState::Leased))
            .count();
        if n == 0 { None } else { Some(n.to_string()) }
    }),
    enabled: None,
    run: |s| open_singleton(s, TabKind::Queue),
    category: ActionCategory::Vault,
};

static A_VAULT_INDEXER: Action = Action {
    id: "vault.open_indexer",
    icon: icons::Icon::Brain,
    label: "Index",
    badge: None,
    enabled: None,
    run: |s| open_singleton(s, TabKind::IndexerDetail),
    category: ActionCategory::Vault,
};

static A_VAULT_GRAPH: Action = Action {
    id: "vault.open_graph",
    icon: icons::Icon::Graph,
    label: "Graph",
    badge: None,
    enabled: None,
    run: |s| open_singleton(s, TabKind::Graph),
    category: ActionCategory::Vault,
};

static A_VAULT_PATCH_REVIEW: Action = Action {
    id: "vault.open_patch_review",
    icon: icons::Icon::Check,
    label: "Patch review",
    badge: Some(|state| {
        let n = state.ui_cache.pending_snapshot.len();
        if n == 0 { None } else { Some(n.to_string()) }
    }),
    enabled: None,
    run: |s| open_singleton(s, TabKind::PatchReview),
    category: ActionCategory::Vault,
};

static A_VAULT_CHANGES: Action = Action {
    id: "vault.open_changes",
    icon: icons::Icon::Clock,
    label: "Changes",
    badge: None,
    enabled: None,
    run: |s| open_singleton(s, TabKind::Changes),
    category: ActionCategory::Vault,
};

static A_VAULT_PLUGINS: Action = Action {
    id: "vault.open_plugins",
    icon: icons::Icon::Plugin,
    label: "Plugins",
    badge: None,
    enabled: None,
    run: |s| open_singleton(s, TabKind::Plugins),
    category: ActionCategory::Vault,
};

static A_CHAT_NEW: Action = Action {
    id: "chat.new_session",
    icon: icons::Icon::Plus,
    label: "New chat session",
    badge: None,
    enabled: None,
    run: |state| {
        let vault_root = state.vault_session.vault_root.clone();
        let (model, provider) = state
            .vault_session
            .config
            .read()
            .map(|c| (c.llm.provider.model.clone(), c.llm.provider.backend.clone()))
            .unwrap_or_else(|_| ("stub-model".into(), "stub".into()));
        if let Err(err) = crate::chat::session::create_new(
            &mut state.chat_state.registry,
            &vault_root,
            &model,
            &provider,
        ) {
            state.push_toast(format!("New chat failed: {err}"), ToastLevel::Error);
        } else {
            ensure_panel_visible(state, crate::tab::PANEL_CHAT);
            // Chat lives in the workbench's secondary side bar, not the
            // tile dock. If the user has collapsed that bar, re-show it so
            // the freshly-created session is visible.
            state.workbench.secondary_side_bar.visible = true;
        }
    },
    category: ActionCategory::Chat,
};

static A_VIEW_TOGGLE_HELP: Action = Action {
    id: "view.toggle_help",
    icon: icons::Icon::Info,
    label: "Toggle help overlay",
    badge: None,
    enabled: None,
    run: |s| s.ui.show_help = !s.ui.show_help,
    category: ActionCategory::View,
};

static A_VIEW_TOGGLE_PROFILER: Action = Action {
    id: "view.toggle_profiler",
    icon: icons::Icon::Chart,
    label: "Toggle profiler overlay",
    badge: None,
    enabled: None,
    run: |s| {
        s.ui.show_profiler = !s.ui.show_profiler;
        crate::profiling::set_enabled(s.ui.show_profiler);
    },
    category: ActionCategory::View,
};

static A_VIEW_TOOLBAR_CUSTOMIZE: Action = Action {
    id: "view.toolbar_customize",
    icon: icons::Icon::Wrench,
    label: "Customize toolbars",
    badge: None,
    enabled: None,
    run: |s| s.ui.customize_toolbars = !s.ui.customize_toolbars,
    category: ActionCategory::View,
};

static A_VIEW_TOGGLE_LEFT_SIDEBAR: Action = Action {
    id: "view.toggle_left_sidebar",
    icon: icons::Icon::SidebarLeft,
    label: "Toggle left sidebar",
    badge: None,
    enabled: None,
    run: |s| s.workbench.primary_side_bar.toggle(),
    category: ActionCategory::View,
};

static A_VIEW_TOGGLE_RIGHT_SIDEBAR: Action = Action {
    id: "view.toggle_right_sidebar",
    icon: icons::Icon::SidebarRight,
    label: "Toggle right sidebar",
    badge: None,
    enabled: None,
    run: |s| s.workbench.secondary_side_bar.toggle(),
    category: ActionCategory::View,
};

static A_VIEW_TOOLBAR_RESET: Action = Action {
    id: "view.toolbar_reset",
    icon: icons::Icon::Restore,
    label: "Reset toolbars to default",
    badge: None,
    enabled: None,
    run: |s| {
        s.ui.toolbars = crate::state::Toolbars::default();
        crate::actions::persist_toolbars(s);
    },
    category: ActionCategory::View,
};

static A_FILE_CLOSE_TAB: Action = Action {
    id: "file.close_tab",
    icon: icons::Icon::Close,
    label: "Close active tab",
    badge: None,
    enabled: Some(|s| s.session.active_tab.is_some()),
    run: |s| {
        if let Some(id) = s.session.active_tab {
            crate::editor_pane::close_tab_with_dirty_guard(s, id);
        }
    },
    category: ActionCategory::File,
};

static A_PALETTE_OPEN: Action = Action {
    id: "palette.open",
    icon: icons::Icon::Search,
    label: "Open command palette",
    badge: None,
    enabled: None,
    run: |s| {
        s.ui.palette_open = true;
        s.ui.palette_query.clear();
        s.ui.palette_selected = 0;
    },
    category: ActionCategory::Palette,
};

// ---- Editor / tab commands (also bound to chords; see `keybinds`) -------
//
// These were previously reachable only from their keyboard chord and a
// separate `known_keybindings` table. They now live in the one registry
// so the command palette and toolbar can surface them too, and the
// keybind table just annotates them with a chord.

static A_EDITOR_SAVE: Action = Action {
    id: "editor.save",
    // No dedicated save glyph in the icon set; reuse Check (the
    // "confirm / commit" affordance) rather than ship a new SVG asset.
    icon: icons::Icon::Check,
    label: "Save the active buffer",
    badge: None,
    enabled: Some(|s| crate::keybinds::active_buffer_path(s).is_some()),
    run: |s| {
        let Some(path) = crate::keybinds::active_buffer_path(s) else {
            return;
        };
        if let Err(err) = editor_pane::save_buffer(s, &path) {
            s.push_toast(format!("Save failed: {err}"), ToastLevel::Error);
        }
    },
    category: ActionCategory::Editor,
};

static A_EDITOR_FIND: Action = Action {
    id: "editor.find",
    icon: icons::Icon::Search,
    label: "Find in note",
    badge: None,
    enabled: Some(|s| crate::keybinds::active_buffer_path(s).is_some()),
    run: |s| {
        if let Some(path) = crate::keybinds::active_buffer_path(s) {
            crate::panels::buffer::find::open(s, &path);
        }
    },
    category: ActionCategory::Editor,
};

static A_EDITOR_READER_VIEW: Action = Action {
    id: "editor.reader_view",
    icon: icons::Icon::Info,
    label: "Toggle reader / focus view",
    badge: None,
    enabled: Some(|s| crate::keybinds::active_buffer_path(s).is_some()),
    run: |s| {
        if let Some(path) = crate::keybinds::active_buffer_path(s)
            && let Some(b) = s.session.buffers.get_mut(&path)
        {
            b.reader_view = !b.reader_view;
        }
    },
    category: ActionCategory::Editor,
};

static A_TAB_CYCLE_NEXT: Action = Action {
    id: "tab.cycle_next",
    icon: icons::Icon::Forward,
    label: "Cycle to the next tab",
    badge: None,
    enabled: Some(|s| s.session.active_tab.is_some()),
    run: |s| crate::keybinds::cycle_active(s, 1),
    category: ActionCategory::Tab,
};

static A_TAB_CYCLE_PREV: Action = Action {
    id: "tab.cycle_prev",
    icon: icons::Icon::Back,
    label: "Cycle to the previous tab",
    badge: None,
    enabled: Some(|s| s.session.active_tab.is_some()),
    run: |s| crate::keybinds::cycle_active(s, -1),
    category: ActionCategory::Tab,
};

static A_VAULT_FOCUS_SEARCH: Action = Action {
    id: "vault.focus_search",
    icon: icons::Icon::Search,
    label: "Focus the search box",
    badge: None,
    enabled: None,
    run: |s| s.search_state.focus_query_next_frame = true,
    category: ActionCategory::Vault,
};

// `tab.jump_1..9` — jump to the Nth tab. One discrete action per slot so
// each is independently reachable from the palette and bound to its own
// `Mod-N` chord (slot 9 jumps to the last tab regardless of count, matching
// the chord handler). Generated via a macro to avoid nine near-identical
// blocks.
macro_rules! jump_action {
    ($name:ident, $id:literal, $label:literal, $idx:literal, $last:literal) => {
        static $name: Action = Action {
            id: $id,
            icon: icons::Icon::Clipboard,
            label: $label,
            badge: None,
            enabled: Some(|s| s.session.active_tab.is_some()),
            run: |s| s.jump_to_tab($idx, $last),
            category: ActionCategory::Tab,
        };
    };
}

jump_action!(A_TAB_JUMP_1, "tab.jump_1", "Jump to the 1st tab", 0, false);
jump_action!(A_TAB_JUMP_2, "tab.jump_2", "Jump to the 2nd tab", 1, false);
jump_action!(A_TAB_JUMP_3, "tab.jump_3", "Jump to the 3rd tab", 2, false);
jump_action!(A_TAB_JUMP_4, "tab.jump_4", "Jump to the 4th tab", 3, false);
jump_action!(A_TAB_JUMP_5, "tab.jump_5", "Jump to the 5th tab", 4, false);
jump_action!(A_TAB_JUMP_6, "tab.jump_6", "Jump to the 6th tab", 5, false);
jump_action!(A_TAB_JUMP_7, "tab.jump_7", "Jump to the 7th tab", 6, false);
jump_action!(A_TAB_JUMP_8, "tab.jump_8", "Jump to the 8th tab", 7, false);
jump_action!(A_TAB_JUMP_9, "tab.jump_9", "Jump to the last tab", 8, true);

// ---- Panel toggles ------------------------------------------------------

/// Toggle a registered side-bar panel via the workbench. For a panel
/// that maps to an activity-bar mode (Files/Clusters/Trails/Search/
/// Related/Backlinks/Vault/Trash) this collapses the primary side bar
/// when that mode is already showing, and otherwise selects the mode +
/// shows the bar. `PANEL_CHAT` lives in the secondary side bar, so its
/// toggle flips that bar's visibility.
fn toggle_panel(state: &mut AppState, panel_id: &'static str) {
    if panel_id == crate::tab::PANEL_CHAT {
        state.workbench.secondary_side_bar.toggle();
        return;
    }
    // The panel id IS the feature id / activity-bar mode. Skip if it
    // doesn't resolve to a primary-activity feature (e.g. chat handled
    // above, or an unknown id).
    if state
        .features
        .by_id(panel_id)
        .is_none_or(|f| !f.primary_activity())
    {
        return;
    }
    let mode = panel_id.to_string();
    let already_showing = state.workbench.primary_side_bar.visible
        && state.workbench.activity_bar.active() == Some(&mode);
    if already_showing {
        state.workbench.primary_side_bar.visible = false;
    } else {
        state.workbench.activity_bar.set_active(Some(mode));
        state.workbench.primary_side_bar.visible = true;
    }
}

/// Reveal a registered side-bar panel via the workbench: select its
/// activity-bar mode and show the primary side bar (or show the
/// secondary side bar for `PANEL_CHAT`). Used by callers that want a
/// specific panel visible after an action (e.g. reveal-in-files,
/// activate-trail, new-chat).
pub fn ensure_panel_visible(state: &mut AppState, panel_id: &'static str) {
    if panel_id == crate::tab::PANEL_CHAT {
        state.workbench.secondary_side_bar.visible = true;
        return;
    }
    if state
        .features
        .by_id(panel_id)
        .is_some_and(|f| f.primary_activity())
    {
        state.workbench.activity_bar.set_active(Some(panel_id.to_string()));
        state.workbench.primary_side_bar.visible = true;
    }
}

static A_PANEL_TOGGLE_FILES: Action = Action {
    id: "panel.toggle.files",
    icon: icons::Icon::Folder,
    label: "Toggle Files panel",
    badge: None,
    enabled: None,
    run: |s| toggle_panel(s, crate::tab::PANEL_FILES),
    category: ActionCategory::Panel,
};
static A_PANEL_TOGGLE_CLUSTERS: Action = Action {
    id: "panel.toggle.clusters",
    icon: icons::Icon::ClusterTree,
    label: "Toggle Clusters panel",
    badge: None,
    enabled: None,
    run: |s| toggle_panel(s, crate::tab::PANEL_CLUSTERS),
    category: ActionCategory::Panel,
};
static A_PANEL_TOGGLE_TRAILS: Action = Action {
    id: "panel.toggle.trails",
    icon: icons::Icon::Boot,
    label: "Toggle Trails panel",
    badge: None,
    enabled: None,
    run: |s| toggle_panel(s, crate::tab::PANEL_TRAILS),
    category: ActionCategory::Panel,
};
static A_PANEL_TOGGLE_SEARCH: Action = Action {
    id: "panel.toggle.search",
    icon: icons::Icon::Search,
    label: "Toggle Search panel",
    badge: None,
    enabled: None,
    run: |s| toggle_panel(s, crate::tab::PANEL_SEARCH),
    category: ActionCategory::Panel,
};
static A_PANEL_TOGGLE_RELATED: Action = Action {
    id: "panel.toggle.related",
    icon: icons::Icon::Graph,
    label: "Toggle Related panel",
    badge: None,
    enabled: None,
    run: |s| toggle_panel(s, crate::tab::PANEL_RELATED),
    category: ActionCategory::Panel,
};
static A_PANEL_TOGGLE_BACKLINKS: Action = Action {
    id: "panel.toggle.backlinks",
    icon: icons::Icon::Graph,
    label: "Toggle Backlinks panel",
    badge: None,
    enabled: None,
    run: |s| toggle_panel(s, crate::tab::PANEL_BACKLINKS),
    category: ActionCategory::Panel,
};
static A_PANEL_TOGGLE_CHAT: Action = Action {
    id: "panel.toggle.chat",
    icon: icons::Icon::Chat,
    label: "Toggle Chat panel",
    badge: None,
    enabled: None,
    run: |s| toggle_panel(s, crate::tab::PANEL_CHAT),
    category: ActionCategory::Panel,
};

static ALL: &[&Action] = &[
    &A_NAV_BACK,
    &A_NAV_FORWARD,
    &A_NAV_HOME,
    &A_VAULT_SWITCH,
    &A_VAULT_SETTINGS,
    &A_VAULT_QUEUE,
    &A_VAULT_INDEXER,
    &A_VAULT_GRAPH,
    &A_VAULT_PATCH_REVIEW,
    &A_VAULT_CHANGES,
    &A_VAULT_PLUGINS,
    &A_CHAT_NEW,
    &A_PANEL_TOGGLE_FILES,
    &A_PANEL_TOGGLE_CLUSTERS,
    &A_PANEL_TOGGLE_TRAILS,
    &A_PANEL_TOGGLE_SEARCH,
    &A_PANEL_TOGGLE_RELATED,
    &A_PANEL_TOGGLE_BACKLINKS,
    &A_PANEL_TOGGLE_CHAT,
    &A_VIEW_TOGGLE_HELP,
    &A_VIEW_TOGGLE_PROFILER,
    &A_VIEW_TOGGLE_LEFT_SIDEBAR,
    &A_VIEW_TOGGLE_RIGHT_SIDEBAR,
    &A_VIEW_TOOLBAR_CUSTOMIZE,
    &A_VIEW_TOOLBAR_RESET,
    &A_FILE_CLOSE_TAB,
    &A_PALETTE_OPEN,
    &A_EDITOR_SAVE,
    &A_EDITOR_FIND,
    &A_EDITOR_READER_VIEW,
    &A_TAB_CYCLE_NEXT,
    &A_TAB_CYCLE_PREV,
    &A_VAULT_FOCUS_SEARCH,
    &A_TAB_JUMP_1,
    &A_TAB_JUMP_2,
    &A_TAB_JUMP_3,
    &A_TAB_JUMP_4,
    &A_TAB_JUMP_5,
    &A_TAB_JUMP_6,
    &A_TAB_JUMP_7,
    &A_TAB_JUMP_8,
    &A_TAB_JUMP_9,
];

static REGISTRY: LazyLock<ActionRegistry> = LazyLock::new(|| {
    let actions: Vec<&'static Action> = ALL.to_vec();
    let by_id = actions.iter().map(|a| (a.id, *a)).collect();
    ActionRegistry { actions, by_id }
});

/// Persist toolbar layout to `<vault>/.hiker/toolbars.json`. Best-effort;
/// logs but does not toast on failure (would be too noisy during customize
/// drag).
pub fn persist_toolbars(state: &AppState) {
    let root = state.vault_session.vault_root.join(".hiker");
    if let Err(err) = std::fs::create_dir_all(&root) {
        tracing::warn!(error = %err, "toolbars persist: create_dir_all .hiker failed");
        return;
    }
    let path = root.join("toolbars.json");
    match serde_json::to_string_pretty(&state.ui.toolbars) {
        Ok(body) => {
            if let Err(err) = std::fs::write(&path, body) {
                tracing::warn!(error = %err, "toolbars persist: write failed");
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "toolbars persist: serialize failed");
        }
    }
}
