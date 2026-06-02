//! Vault view — a read-only *logical lens* over the vault's notes,
//! offered as a sidebar mode alongside the literal on-disk Files tree.
//! See `docs/vault-view.md`.
//!
//! Files mode shows where bytes live (the real, nested directory tree);
//! Vault mode shows how the knowledge is organized — derived from the
//! index + frontmatter, never moving a file. The default `Composed` lens
//! nests crawl/feed children under their job note
//! (`vault-view-crawl-nesting`), trail waypoints under their trail-doc
//! (`vault-view-trail-nesting`), extracted sidecars under their source
//! (`vault-view-sidecar-surfacing`), and groups the rest by source-type /
//! authorship (`vault-view-source-groups`). The nesting authority (parent
//! stamp / resolved waypoint tree, not folder membership) lives in
//! `tree.rs`; the simpler by-folder / flat lenses remain selectable. The
//! relationship/provenance metadata is read in one cheap query
//! (`Store::notes_with_meta`, projected from the `note_meta` index) so the
//! lens never touches frontmatter on disk per render.
//!
//! Migrated off `panels_registry` to a real `Feature` (`Vault`) whose
//! `SidebarSurface` renders through the narrow `feature::Ctx`: note paths
//! come from `ctx.services.read_store`, lens/collapse state from
//! `ctx.state`, and opening a note is deferred via `ctx.defer`. The lens
//! picker stays a free `actions_menu` invoked from the workbench `⋯`
//! menu. status: vault-view-mode

use eframe::egui;

use crate::editor_pane;
use crate::feature::{Ctx, Feature, SidebarSurface};
use crate::icons;
use crate::state::AppState;

mod tree;
use tree::{NodeKind, VaultNode};

/// Which derived grouping the Vault lens renders. Display state only —
/// never stored on a note (`vault-view-readonly-lens`). Selectable from
/// the mode's `⋯` menu.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Lens {
    /// The composed default: crawl/feed children nested under their job
    /// note (`vault-view-crawl-nesting`), trail waypoints under their
    /// trail-doc (`vault-view-trail-nesting`), extracted sidecars under
    /// their source (`vault-view-sidecar-surfacing`), and everything else
    /// grouped by source-type / authorship (`vault-view-source-groups`).
    #[default]
    Composed,
    /// Notes grouped under a virtual node per top-level folder, flattened
    /// within each (distinct from Files' fully-nested tree).
    ByFolder,
    /// Every note in one flat, alphabetically-sorted list.
    Flat,
}

/// Per-session Vault-view UI state. In-memory; the lens choice is display
/// state per `vault-view-readonly-lens`. Owned by `AppState::vault_state`
/// (top-level, per `feature-state-ownership`).
#[derive(Default)]
pub struct State {
    pub lens: Lens,
    /// Collapsed group keys (by-folder lens). Absent = expanded.
    pub collapsed: std::collections::HashSet<String>,
}

/// Render the read-only derived tree through the narrow feature `Ctx`:
/// note paths from the read store, lens/collapse from `ctx.state`,
/// open-note via `ctx.defer`. Never mutates placement.
fn render_body(ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
    let lens = ctx.state.downcast_ref::<State>().expect("vault state").lens;
    if lens == Lens::Composed {
        render_composed(ui, ctx);
        return;
    }
    let paths = match ctx.services.read_store.lock() {
        Ok(s) => s.all_note_paths().unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    if paths.is_empty() {
        ui.add_space(8.0);
        ui.weak("No indexed notes yet.");
        return;
    }
    match lens {
        Lens::Flat => render_flat(ui, ctx, paths),
        Lens::ByFolder => render_by_folder(ui, ctx, paths),
        Lens::Composed => unreachable!("handled above"),
    }
}

/// The composed lens: build the derived forest from the store's
/// relationship/provenance projection + resolved waypoint rows, then walk
/// it. Tree construction (the nesting authority) lives in `tree.rs`; this is
/// the egui paint over its output.
fn render_composed(ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
    let (notes, waypoints) = match ctx.services.read_store.lock() {
        Ok(s) => (
            s.notes_with_meta().unwrap_or_default(),
            s.all_trail_waypoints().unwrap_or_default(),
        ),
        Err(_) => (Vec::new(), Vec::new()),
    };
    if notes.is_empty() {
        ui.add_space(8.0);
        ui.weak("No indexed notes yet.");
        return;
    }
    let forest = tree::build_composed(&notes, &waypoints);
    for node in &forest {
        render_node(ui, ctx, node, 0);
    }
}

/// Icon for a node, by its derived kind.
fn node_icon(kind: NodeKind) -> egui::Image<'static> {
    let icon = match kind {
        NodeKind::Group => icons::Icon::Folder,
        NodeKind::Capture => icons::Icon::Compass,
        NodeKind::Trail => icons::Icon::Boot,
        NodeKind::Waypoint => icons::Icon::Bookmark,
        NodeKind::Session => icons::Icon::Chat,
        NodeKind::Note => icons::Icon::File,
    };
    icons::ICONS.image(icon)
}

/// Recursively render one derived node. Nodes with children get a collapse
/// chevron (keyed by path or label); leaves open their note on click.
/// Read-only: no drag, no placement mutation (`vault-view-readonly-lens`).
fn render_node(ui: &mut egui::Ui, ctx: &mut Ctx<'_>, node: &VaultNode, depth: usize) {
    let indent = 8.0 + depth as f32 * 14.0;
    let has_children = !node.children.is_empty();
    let key = node.path.clone().unwrap_or_else(|| format!("group:{}", node.label));
    let collapsed = ctx
        .state
        .downcast_ref::<State>()
        .expect("vault state")
        .collapsed
        .contains(&key);

    ui.horizontal(|ui| {
        ui.add_space(indent);
        if has_children {
            let chevron = if collapsed {
                icons::ICONS.image(icons::Icon::ChevronRight)
            } else {
                icons::ICONS.image(icons::Icon::ChevronDown)
            };
            if ui.add(egui::Button::image(chevron).frame(false)).clicked() {
                let set = &mut ctx.state.downcast_mut::<State>().expect("vault state").collapsed;
                if collapsed {
                    set.remove(&key);
                } else {
                    set.insert(key.clone());
                }
            }
        } else {
            ui.add_space(16.0);
        }
        let btn = egui::Button::image_and_text(node_icon(node.kind), node.label.clone())
            .frame(false);
        let resp = ui.add(btn);
        let resp = match &node.path {
            Some(p) => resp.on_hover_text(p.clone()),
            None => resp,
        };
        if resp.clicked()
            && let Some(p) = node.path.clone()
        {
            let sticky = ui.input(|i| i.modifiers.command);
            ctx.defer(move |app| editor_pane::open_file(app, &p, sticky));
        }
    });

    if has_children && !collapsed {
        for child in &node.children {
            render_node(ui, ctx, child, depth + 1);
        }
    }
}

fn render_flat(ui: &mut egui::Ui, ctx: &mut Ctx<'_>, mut paths: Vec<String>) {
    paths.sort_by(|a, b| basename(a).cmp(basename(b)));
    for rel in paths {
        note_row(ui, ctx, &rel, 0);
    }
}

fn render_by_folder(ui: &mut egui::Ui, ctx: &mut Ctx<'_>, paths: Vec<String>) {
    // Group by top-level folder segment; root notes under "(root)".
    let mut groups: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for rel in paths {
        groups.entry(top_group(&rel)).or_default().push(rel);
    }
    for (group, mut members) in groups {
        members.sort_by(|a, b| basename(a).cmp(basename(b)));
        let collapsed = ctx
            .state
            .downcast_ref::<State>()
            .expect("vault state")
            .collapsed
            .contains(&group);
        let chevron = if collapsed {
            icons::ICONS.image(icons::Icon::ChevronRight)
        } else {
            icons::ICONS.image(icons::Icon::ChevronDown)
        };
        let header = format!("{group}  ({})", members.len());
        if ui
            .add(egui::Button::image_and_text(chevron, header).frame(false))
            .clicked()
        {
            let set = &mut ctx.state.downcast_mut::<State>().expect("vault state").collapsed;
            if collapsed {
                set.remove(&group);
            } else {
                set.insert(group.clone());
            }
        }
        if !collapsed {
            for rel in members {
                note_row(ui, ctx, &rel, 1);
            }
        }
    }
}

/// One clickable, read-only note row. Click opens in the preview slot
/// (`editor-preview-tab-from-open-callsites`); Mod-click opens sticky.
fn note_row(ui: &mut egui::Ui, ctx: &mut Ctx<'_>, rel: &str, depth: usize) {
    let indent = 8.0 + depth as f32 * 14.0;
    ui.horizontal(|ui| {
        ui.add_space(indent);
        let resp = ui
            .add(
                egui::Button::image_and_text(
                    icons::ICONS.image(icons::Icon::File),
                    basename(rel),
                )
                .frame(false),
            )
            .on_hover_text(rel);
        if resp.clicked() {
            let sticky = ui.input(|i| i.modifiers.command);
            let rel_owned = rel.to_string();
            ctx.defer(move |app| editor_pane::open_file(app, &rel_owned, sticky));
        }
    });
}

/// Basename without the indexable extension, for display.
fn basename(rel: &str) -> &str {
    rel.rsplit('/')
        .next()
        .unwrap_or(rel)
        .trim_end_matches(".md")
        .trim_end_matches(".markdown")
        .trim_end_matches(".txt")
}

/// Top-level folder group key for the by-folder lens. A path with no
/// `/` is a root note → `(root)`; otherwise the first path segment.
fn top_group(rel: &str) -> String {
    match rel.split_once('/') {
        Some((top, _)) => top.to_string(),
        None => "(root)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{basename, top_group};

    #[test]
    fn basename_strips_dir_and_ext() {
        assert_eq!(basename("research/embeddings/whisper.md"), "whisper");
        assert_eq!(basename("note.txt"), "note");
        assert_eq!(basename("plain"), "plain");
    }

    #[test]
    fn top_group_uses_first_segment_or_root() {
        assert_eq!(top_group("research/x/y.md"), "research");
        assert_eq!(top_group("inbox/a.md"), "inbox");
        assert_eq!(top_group("toplevel.md"), "(root)");
    }
}

/// `⋯`-menu entries for Vault mode: the lens picker. Invoked from
/// `workbench_host`'s `side_bar_actions_menu` for the Vault mode (which
/// has full `&mut AppState`, unlike the registry sidebar render path).
/// status: vault-view-mode
pub fn actions_menu(ui: &mut egui::Ui, app: &mut AppState) {
    ui.label(
        egui::RichText::new("Group by")
            .color(hiker_theme::muted())
            .small(),
    );
    let cur = app.vault_state.lens;
    for (label, lens) in [
        ("Relationships", Lens::Composed),
        ("Folder", Lens::ByFolder),
        ("Flat (all notes)", Lens::Flat),
    ] {
        let prefix = if cur == lens { "* " } else { "  " };
        if ui.button(format!("{prefix}{label}")).clicked() {
            app.vault_state.lens = lens;
            ui.close();
        }
    }
}

// ---- Feature impl ----------------------------------------------------

/// Zero-sized `Feature` descriptor for the Vault lens. State lives in
/// `AppState::vault_state`; the surface reaches it via
/// `Ctx::state.downcast_mut::<State>()`.
pub struct Vault;

impl Feature for Vault {
    fn id(&self) -> &'static str {
        "vault"
    }
    fn label(&self) -> &'static str {
        "Vault"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::Vault)
    }
    fn sidebar(&self) -> Option<&dyn SidebarSurface> {
        Some(&VaultSidebar)
    }
}

struct VaultSidebar;

impl SidebarSurface for VaultSidebar {
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
        egui::ScrollArea::vertical()
            .id_salt("panel-vault-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                render_body(ui, ctx);
            });
    }
}
