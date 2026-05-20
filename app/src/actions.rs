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

use eframe::egui;

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
    Chat,
    Palette,
    Panel,
    Layout,
}

impl ActionCategory {
    pub fn label(self) -> &'static str {
        match self {
            ActionCategory::Navigation => "Navigation",
            ActionCategory::View => "View",
            ActionCategory::Vault => "Vault",
            ActionCategory::File => "File",
            ActionCategory::Chat => "Chat",
            ActionCategory::Palette => "Palette",
            ActionCategory::Panel => "Panel",
            ActionCategory::Layout => "Layout",
        }
    }
}

pub struct Action {
    pub id: ActionId,
    pub icon: fn() -> egui::Image<'static>,
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
    icon: icons::back,
    label: "Back",
    badge: None,
    enabled: Some(nav_can_back),
    run: |s| editor_pane::nav_go(s, -1),
    category: ActionCategory::Navigation,
};

static A_NAV_FORWARD: Action = Action {
    id: "nav.forward",
    icon: icons::forward,
    label: "Forward",
    badge: None,
    enabled: Some(nav_can_forward),
    run: |s| editor_pane::nav_go(s, 1),
    category: ActionCategory::Navigation,
};

static A_NAV_HOME: Action = Action {
    id: "nav.home",
    icon: icons::home,
    label: "Home",
    badge: None,
    enabled: None,
    run: |s| open_singleton(s, TabKind::Home),
    category: ActionCategory::Navigation,
};

static A_VAULT_SWITCH: Action = Action {
    id: "vault.switch",
    icon: icons::folder,
    label: "Open vault",
    badge: None,
    enabled: None,
    run: pick_vault,
    category: ActionCategory::Vault,
};

fn pick_vault(state: &mut AppState) {
    let Some(path) = rfd::FileDialog::new()
        .set_title("Pick a vault folder")
        .set_directory(&state.vault_session.vault_root)
        .pick_folder()
    else {
        return;
    };
    if path == state.vault_session.vault_root {
        return;
    }
    crate::toolbar::queue_vault_switch(state, path);
}

static A_VAULT_SETTINGS: Action = Action {
    id: "vault.open_settings",
    icon: icons::settings,
    label: "Settings",
    badge: None,
    enabled: None,
    run: |s| open_singleton(s, TabKind::Settings),
    category: ActionCategory::Vault,
};

fn queue_badge(state: &AppState) -> Option<String> {
    use hiker_core::tasks::TaskState;
    let n = state
        .ui_cache
        .task_snapshot
        .iter()
        .filter(|r| matches!(r.state, TaskState::Queued | TaskState::Leased))
        .count();
    if n == 0 { None } else { Some(n.to_string()) }
}

fn staging_badge(state: &AppState) -> Option<String> {
    let n = state.ui_cache.staging_snapshot.len();
    if n == 0 { None } else { Some(n.to_string()) }
}

static A_VAULT_QUEUE: Action = Action {
    id: "vault.open_queue",
    icon: icons::clipboard,
    label: "Queue",
    badge: Some(queue_badge),
    enabled: None,
    run: |s| open_singleton(s, TabKind::Queue),
    category: ActionCategory::Vault,
};

static A_VAULT_INDEXER: Action = Action {
    id: "vault.open_indexer",
    icon: icons::brain,
    label: "Index",
    badge: None,
    enabled: None,
    run: |s| open_singleton(s, TabKind::IndexerDetail),
    category: ActionCategory::Vault,
};

static A_VAULT_GRAPH: Action = Action {
    id: "vault.open_graph",
    icon: icons::graph,
    label: "Graph",
    badge: None,
    enabled: None,
    run: |s| open_singleton(s, TabKind::Graph),
    category: ActionCategory::Vault,
};

static A_VAULT_PATCH_REVIEW: Action = Action {
    id: "vault.open_patch_review",
    icon: icons::check,
    label: "Patch review",
    badge: Some(staging_badge),
    enabled: None,
    run: |s| open_singleton(s, TabKind::PatchReview),
    category: ActionCategory::Vault,
};

static A_VAULT_AGENT_CHANGES: Action = Action {
    id: "vault.open_agent_changes",
    icon: icons::robot,
    label: "Agent changes",
    badge: None,
    enabled: None,
    run: |s| open_singleton(s, TabKind::AgentChanges),
    category: ActionCategory::Vault,
};

static A_VAULT_PLUGINS: Action = Action {
    id: "vault.open_plugins",
    icon: icons::plugin,
    label: "Plugins",
    badge: None,
    enabled: None,
    run: |s| open_singleton(s, TabKind::Plugins),
    category: ActionCategory::Vault,
};

static A_CHAT_NEW: Action = Action {
    id: "chat.new_session",
    icon: icons::plus,
    label: "New chat session",
    badge: None,
    enabled: None,
    run: new_chat_session,
    category: ActionCategory::Chat,
};

fn new_chat_session(state: &mut AppState) {
    let vault_root = state.vault_session.vault_root.clone();
    let (model, provider) = state
        .vault_session
        .config
        .read()
        .map(|c| (c.llm.provider.model.clone(), c.llm.provider.backend.clone()))
        .unwrap_or_else(|_| ("stub-model".into(), "stub".into()));
    if let Err(err) = crate::chat::session::create_new(
        &mut state.session.chat,
        &vault_root,
        &model,
        &provider,
    ) {
        state.push_toast(format!("New chat failed: {err}"), ToastLevel::Error);
    } else {
        ensure_panel_visible(state, crate::panels_registry::PANEL_CHAT);
    }
}

static A_VIEW_TOGGLE_HELP: Action = Action {
    id: "view.toggle_help",
    icon: icons::info,
    label: "Toggle help overlay",
    badge: None,
    enabled: None,
    run: |s| s.ui.show_help = !s.ui.show_help,
    category: ActionCategory::View,
};

static A_VIEW_TOGGLE_PROFILER: Action = Action {
    id: "view.toggle_profiler",
    icon: icons::chart,
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
    icon: icons::wrench,
    label: "Customize toolbars",
    badge: None,
    enabled: None,
    run: |s| s.ui.customize_toolbars = !s.ui.customize_toolbars,
    category: ActionCategory::View,
};

static A_VIEW_TOOLBAR_RESET: Action = Action {
    id: "view.toolbar_reset",
    icon: icons::restore,
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
    icon: icons::close,
    label: "Close active tab",
    badge: None,
    enabled: Some(|s| s.session.active_tab.is_some()),
    run: |s| {
        if let Some(id) = s.session.active_tab {
            crate::tabs::close_tab_with_dirty_guard(s, id);
        }
    },
    category: ActionCategory::File,
};

static A_PALETTE_OPEN: Action = Action {
    id: "palette.open",
    icon: icons::search,
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

// ---- Panel toggles ------------------------------------------------------

fn toggle_panel(state: &mut AppState, panel_id: &'static str) {
    if let Some(tile_id) = crate::layout::find_panel_tile(&state.session.dock, panel_id) {
        state.session.dock.remove_recursively(tile_id);
        state.session.dock_dirty = true;
        return;
    }
    ensure_panel_visible(state, panel_id);
}

/// Insert `panel_id` into the dock if it's not already present, then
/// activate its tab. Re-inserts at the panel's last-known TileId if we
/// remember a still-valid one; otherwise falls back to the panel's
/// default side.
pub fn ensure_panel_visible(state: &mut AppState, panel_id: &'static str) {
    use crate::tab::DockTab;
    use egui_tiles::{Container, Tile};
    if let Some(tile_id) = crate::layout::find_panel_tile(&state.session.dock, panel_id) {
        // Already in the dock; activate it by making its ancestor tabs
        // container show this pane.
        state
            .session
            .dock
            .make_active(|id, _tile| id == tile_id);
        return;
    }

    // Look up the panel's default side so we know where it wants to go
    // if we don't remember anything.
    let reg = crate::panels_registry::PanelRegistry::all();
    let default_side = reg.by_id(panel_id).map(|p| p.default_side);

    // Prefer the last-known container if it still exists and is Tabs.
    let target = state
        .session
        .panel_locations
        .get(panel_id)
        .copied()
        .filter(|tid| {
            matches!(
                state.session.dock.tiles.get(*tid),
                Some(Tile::Container(Container::Tabs(_)))
            )
        })
        .unwrap_or(match default_side {
            Some(crate::panels_registry::PanelSide::Left) => state.session.left_tile,
            Some(crate::panels_registry::PanelSide::Right) => state.session.right_tile,
            _ => state.session.center_tile,
        });

    let pane_id = state
        .session
        .dock
        .tiles
        .insert_pane(DockTab::panel(panel_id));
    if let Some(Tile::Container(Container::Tabs(tabs))) =
        state.session.dock.tiles.get_mut(target)
    {
        tabs.add_child(pane_id);
        tabs.set_active(pane_id);
    } else {
        // Target container is gone; drop the pane (don't leak it) and
        // fall through silently. The next reconcile cycle won't see it.
        state.session.dock.tiles.remove(pane_id);
        return;
    }
    state.session.dock_dirty = true;
}

static A_PANEL_TOGGLE_FILES: Action = Action {
    id: "panel.toggle.files",
    icon: icons::folder,
    label: "Toggle Files panel",
    badge: None,
    enabled: None,
    run: |s| toggle_panel(s, crate::panels_registry::PANEL_FILES),
    category: ActionCategory::Panel,
};
static A_PANEL_TOGGLE_CLUSTERS: Action = Action {
    id: "panel.toggle.clusters",
    icon: icons::cluster_tree,
    label: "Toggle Clusters panel",
    badge: None,
    enabled: None,
    run: |s| toggle_panel(s, crate::panels_registry::PANEL_CLUSTERS),
    category: ActionCategory::Panel,
};
static A_PANEL_TOGGLE_TRAILS: Action = Action {
    id: "panel.toggle.trails",
    icon: icons::boot,
    label: "Toggle Trails panel",
    badge: None,
    enabled: None,
    run: |s| toggle_panel(s, crate::panels_registry::PANEL_TRAILS),
    category: ActionCategory::Panel,
};
static A_PANEL_TOGGLE_SEARCH: Action = Action {
    id: "panel.toggle.search",
    icon: icons::search,
    label: "Toggle Search panel",
    badge: None,
    enabled: None,
    run: |s| toggle_panel(s, crate::panels_registry::PANEL_SEARCH),
    category: ActionCategory::Panel,
};
static A_PANEL_TOGGLE_RELATED: Action = Action {
    id: "panel.toggle.related",
    icon: icons::graph,
    label: "Toggle Related panel",
    badge: None,
    enabled: None,
    run: |s| toggle_panel(s, crate::panels_registry::PANEL_RELATED),
    category: ActionCategory::Panel,
};
static A_PANEL_TOGGLE_BACKLINKS: Action = Action {
    id: "panel.toggle.backlinks",
    icon: icons::graph,
    label: "Toggle Backlinks panel",
    badge: None,
    enabled: None,
    run: |s| toggle_panel(s, crate::panels_registry::PANEL_BACKLINKS),
    category: ActionCategory::Panel,
};
static A_PANEL_TOGGLE_CHAT: Action = Action {
    id: "panel.toggle.chat",
    icon: icons::chat,
    label: "Toggle Chat panel",
    badge: None,
    enabled: None,
    run: |s| toggle_panel(s, crate::panels_registry::PANEL_CHAT),
    category: ActionCategory::Panel,
};

// ---- Layout actions -----------------------------------------------------

fn current_bundle(state: &AppState) -> crate::layout::DockBundle {
    crate::layout::DockBundle {
        tree: state.session.dock.clone(),
        center_tile: state.session.center_tile,
        left_tile: state.session.left_tile,
        right_tile: state.session.right_tile,
    }
}

fn apply_bundle(state: &mut AppState, bundle: crate::layout::DockBundle) {
    state.session.dock = bundle.tree;
    state.session.center_tile = bundle.center_tile;
    state.session.left_tile = bundle.left_tile;
    state.session.right_tile = bundle.right_tile;
    state.session.dock_dirty = true;
}

fn layout_save_as(state: &mut AppState) {
    let Some(path) = rfd::FileDialog::new()
        .set_title("Save layout profile")
        .set_directory(crate::layout::user_profiles_dir())
        .set_file_name("my-layout.json")
        .add_filter("Layout JSON", &["json"])
        .save_file()
    else {
        return;
    };
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("layout")
        .to_string();
    let bundle = current_bundle(state);
    match crate::layout::save_profile(&stem, &bundle) {
        Ok(p) => state.push_toast(
            format!("Layout saved as {}", p.display()),
            ToastLevel::Info,
        ),
        Err(err) => state.push_toast(
            format!("Save layout failed: {err}"),
            ToastLevel::Error,
        ),
    }
}

fn layout_apply(state: &mut AppState) {
    let Some(path) = rfd::FileDialog::new()
        .set_title("Apply layout profile")
        .set_directory(crate::layout::user_profiles_dir())
        .add_filter("Layout JSON", &["json"])
        .pick_file()
    else {
        return;
    };
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("layout")
        .to_string();
    match crate::layout::load_profile(&stem) {
        Some(bundle) => {
            apply_bundle(state, bundle);
            state.push_toast(
                format!("Layout '{stem}' applied"),
                ToastLevel::Info,
            );
        }
        None => state.push_toast(
            format!("Layout '{stem}' not found"),
            ToastLevel::Error,
        ),
    }
}

fn layout_set_as_default(state: &mut AppState) {
    let bundle = current_bundle(state);
    match crate::layout::save_user_default(&bundle) {
        Ok(()) => state.push_toast("Current layout saved as default", ToastLevel::Info),
        Err(err) => state.push_toast(
            format!("Set default failed: {err}"),
            ToastLevel::Error,
        ),
    }
}

fn layout_reset_to_default(state: &mut AppState) {
    let bundle = crate::layout::load_for_vault(&state.vault_session.vault_root);
    apply_bundle(state, bundle);
    state.push_toast("Layout reset to default", ToastLevel::Info);
}

fn layout_reset_factory(state: &mut AppState) {
    let bundle = crate::layout::default_dock();
    apply_bundle(state, bundle);
    state.push_toast("Layout reset to factory default", ToastLevel::Info);
}

static A_LAYOUT_SAVE_AS: Action = Action {
    id: "layout.save_as",
    icon: icons::check,
    label: "Save layout as...",
    badge: None,
    enabled: None,
    run: layout_save_as,
    category: ActionCategory::Layout,
};
static A_LAYOUT_APPLY: Action = Action {
    id: "layout.apply",
    icon: icons::folder,
    label: "Apply layout...",
    badge: None,
    enabled: None,
    run: layout_apply,
    category: ActionCategory::Layout,
};
static A_LAYOUT_SET_AS_DEFAULT: Action = Action {
    id: "layout.set_as_default",
    icon: icons::check,
    label: "Set current layout as default",
    badge: None,
    enabled: None,
    run: layout_set_as_default,
    category: ActionCategory::Layout,
};
static A_LAYOUT_RESET_TO_DEFAULT: Action = Action {
    id: "layout.reset_to_default",
    icon: icons::restore,
    label: "Reset layout to default",
    badge: None,
    enabled: None,
    run: layout_reset_to_default,
    category: ActionCategory::Layout,
};
static A_LAYOUT_RESET_FACTORY: Action = Action {
    id: "layout.reset_factory",
    icon: icons::restore,
    label: "Reset layout to factory",
    badge: None,
    enabled: None,
    run: layout_reset_factory,
    category: ActionCategory::Layout,
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
    &A_VAULT_AGENT_CHANGES,
    &A_VAULT_PLUGINS,
    &A_CHAT_NEW,
    &A_PANEL_TOGGLE_FILES,
    &A_PANEL_TOGGLE_CLUSTERS,
    &A_PANEL_TOGGLE_TRAILS,
    &A_PANEL_TOGGLE_SEARCH,
    &A_PANEL_TOGGLE_RELATED,
    &A_PANEL_TOGGLE_BACKLINKS,
    &A_PANEL_TOGGLE_CHAT,
    &A_LAYOUT_SAVE_AS,
    &A_LAYOUT_APPLY,
    &A_LAYOUT_SET_AS_DEFAULT,
    &A_LAYOUT_RESET_TO_DEFAULT,
    &A_LAYOUT_RESET_FACTORY,
    &A_VIEW_TOGGLE_HELP,
    &A_VIEW_TOGGLE_PROFILER,
    &A_VIEW_TOOLBAR_CUSTOMIZE,
    &A_VIEW_TOOLBAR_RESET,
    &A_FILE_CLOSE_TAB,
    &A_PALETTE_OPEN,
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

/// Load toolbar layout from `<vault>/.hiker/toolbars.json`. Returns the
/// default layout if the file is missing or malformed.
pub fn load_toolbars(vault_root: &std::path::Path) -> crate::state::Toolbars {
    let path = vault_root.join(".hiker/toolbars.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return crate::state::Toolbars::default();
    };
    serde_json::from_slice::<crate::state::Toolbars>(&bytes)
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "toolbars: parse failed; using default");
            crate::state::Toolbars::default()
        })
}
