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
//! Migrated off `panels_registry` to a real `Activity` (`Vault`) whose
//! `View` renders through the narrow `activity::SurfaceCtx`: note paths
//! come from `ctx.services.read_store`, lens/collapse state from
//! `ctx.state`, and opening a note is deferred via `ctx.defer`. The lens
//! picker stays a free `actions_menu` invoked from the workbench `⋯`
//! menu. status: vault-view-mode

use eframe::egui;

use crate::editor_pane;
use egui_workbench::activity::{Activity, View};
use crate::activity::{AppCtx, SurfaceCtx};
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

/// Render the read-only derived tree through the narrow activity `SurfaceCtx`:
/// note paths from the read store, lens/collapse from `ctx.state`,
/// open-note via `ctx.defer`. Never mutates placement.
fn render_body(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>) {
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
fn render_composed(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>) {
    // Smart-folder membership recomputes from the indexed store on render,
    // like the sibling derived projections below — query-docs come from one
    // `hiker.kind = query` lookup, members from `run_query`, never a vault
    // walk (`smart-folder-view`).
    let (notes, waypoints, folders) = match ctx.services.read_store.lock() {
        Ok(s) => (
            s.notes_with_meta().unwrap_or_default(),
            s.all_trail_waypoints().unwrap_or_default(),
            hiker_core::queries::smart_folders(&s, ctx.vault, &ctx.services.kinds)
                .unwrap_or_default(),
        ),
        Err(_) => (Vec::new(), Vec::new(), Vec::new()),
    };
    if notes.is_empty() {
        ui.add_space(8.0);
        ui.weak("No indexed notes yet.");
        return;
    }
    // Cluster-tree note paths (`hiker.kind: cluster-tree`) get a rich force-
    // directed row preview (`vault-view-row-previews`); collect them once.
    let tree_paths: std::collections::HashSet<String> = notes
        .iter()
        .filter(|n| n.kind.as_deref() == Some("cluster-tree"))
        .map(|n| n.path.clone())
        .collect();
    let forest = tree::build_composed(&notes, &waypoints, &folders);
    for node in &forest {
        render_node(ui, ctx, node, 0, &tree_paths);
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
        // The query glyph marks a smart-folder header (`smart-folder-view`).
        NodeKind::Query => icons::Icon::Search,
        NodeKind::QueryMember => icons::Icon::File,
        NodeKind::QueryError => icons::Icon::Warning,
        NodeKind::Note => icons::Icon::File,
    };
    icons::ICONS.image(icon)
}

/// Recursively render one derived node. Nodes with children get a collapse
/// chevron (keyed by path or label); leaves open their note on click.
/// Read-only as a lens (`vault-view-readonly-lens`): rows are drag SOURCES
/// (the uniform vault-path payload, `interaction.md` [drag-note-payload]) but
/// never drop targets — dragging out mutates nothing here.
///
/// A row whose path is a cluster-tree note gets a rich force-directed preview
/// thumbnail before its label (`vault-view-row-previews`); the generic
/// `widgets::preview::thumbnail` widget owns rendering, caching, and the
/// hover-expand.
fn render_node(
    ui: &mut egui::Ui,
    ctx: &mut SurfaceCtx<'_>,
    node: &VaultNode,
    depth: usize,
    tree_paths: &std::collections::HashSet<String>,
) {
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
        if let Some(path) = node.path.as_deref()
            && tree_paths.contains(path)
        {
            row_tree_thumbnail(ui, ctx, path);
        }
        // A smart-folder member is a *virtual* row — the note lives at its
        // real path elsewhere — so it renders italic, with a muted "ref"
        // badge appended below, per the lens's shared not-a-real-residence
        // marking (`smart-folder-view`).
        let label: egui::WidgetText = if node.kind == NodeKind::QueryMember {
            egui::RichText::new(node.label.clone()).italics().into()
        } else {
            node.label.clone().into()
        };
        // Rows with a path open a note, so they carry the full note-row
        // grammar: drag senses for the vault-path payload below, and the
        // pointer cursor pairs with the button's themed hover wash
        // (`interaction.md` [hover-open-signal] / [drag-note-payload]).
        let btn = egui::Button::image_and_text(node_icon(node.kind), label).frame(false);
        let resp = if node.path.is_some() {
            ui.add(btn.sense(egui::Sense::click_and_drag()))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
        } else {
            ui.add(btn)
        };
        if let Some(p) = node.path.as_deref() {
            crate::widgets::note_row::note_drag_source(ui, &resp, p, &node.label);
        }
        if node.kind == NodeKind::QueryMember {
            ui.label(
                egui::RichText::new("ref")
                    .small()
                    .color(hiker_theme::muted()),
            );
        }
        // Exactly one hover affordance per row, so nothing overlaps:
        //  - a regular note row → the rich read-only markdown preview;
        //  - a cluster-tree row → its own inline thumbnail + expand preview;
        //  - a group / non-note source row → the plain path tooltip.
        let path = node.path.as_deref();
        let is_tree = path.is_some_and(|p| tree_paths.contains(p));
        let previewable = path.is_some() && !is_tree && node.kind != NodeKind::Group;
        let resp = if previewable {
            if resp.hovered() {
                crate::widgets::preview::register_note_hover(ui, resp.rect, path.unwrap());
            }
            resp
        } else if let Some(p) = path.filter(|_| !is_tree) {
            resp.on_hover_text(p.to_owned())
        } else {
            resp
        };
        if let Some(p) = node.path.as_deref() {
            // A smart-folder header composes the host-contextual scoped-graph
            // verb onto the base; every other row keeps the plain base menu.
            // status: graph-scoped-query
            if node.kind == NodeKind::Query {
                query_header_menu(&resp, ctx, p);
            } else {
                crate::item_menu::attach_note_item_menu(
                    &resp,
                    ctx,
                    p,
                    crate::item_menu::BaseOpts { reveal: true },
                );
            }
        }
        if resp.clicked()
            && let Some(p) = node.path.clone()
        {
            let sticky = crate::widgets::note_row::open_sticky(ui.input(|i| i.modifiers));
            ctx.defer(move |app| editor_pane::open_file(app, &p, sticky));
        }
    });

    if has_children && !collapsed {
        for child in &node.children {
            render_node(ui, ctx, child, depth + 1, tree_paths);
        }
    }
}

/// The smart-folder header's context menu: the shared note-item base plus
/// the host-contextual "Open in graph, scoped" verb — the vault graph
/// bounded to this query's match set (`graph-scoped-query`; the contextual
/// composition `ctxmenu-contextual-extend` exists for). Dispatch defers
/// through `ctx`, the standard app-surface path.
fn query_header_menu(resp: &egui::Response, ctx: &mut SurfaceCtx<'_>, path: &str) {
    enum Verb {
        Base(crate::item_menu::ItemAction),
        OpenScoped,
    }
    let mut chosen = None;
    resp.context_menu(|ui| {
        let menu = crate::item_menu::note_item_base(
            path,
            crate::item_menu::BaseOpts { reveal: true },
            Verb::Base,
        )
        .section()
        .action("Open in graph, scoped", Verb::OpenScoped);
        chosen = egui_workbench::menu::show(ui, menu);
    });
    if let Some(verb) = chosen {
        let owned = path.to_owned();
        ctx.defer(move |app| match verb {
            Verb::Base(a) => crate::item_menu::apply_item_action(app, a, &owned),
            Verb::OpenScoped => crate::panels::graph::open_scoped(app, &owned),
        });
    }
}

/// Render the inline cluster-tree preview thumbnail for a row. The tree id is
/// the note's filename stem (`{dir}/<tree-id>.md`, per `cluster-tree-visible-note`);
/// its nodes are loaded read-only via `trees.list_nodes`. A load failure draws
/// nothing — the row still renders its label normally.
/// status: vault-view-row-previews
fn row_tree_thumbnail(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>, path: &str) {
    let Some(tree_id) = path.rsplit('/').next().and_then(|b| b.strip_suffix(".md")) else {
        return;
    };
    let Ok(nodes) = ctx.services.trees.list_nodes(tree_id) else {
        return;
    };
    if nodes.is_empty() {
        return;
    }
    let provider = crate::panels::cluster_thumbnail::TreeThumbnail::new(&nodes);
    crate::widgets::preview::thumbnail(
        ui,
        &provider,
        ctx.vault.root(),
        crate::widgets::preview::ThumbnailOpts::default(),
    );
    ui.add_space(4.0);
}

fn render_flat(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>, mut paths: Vec<String>) {
    paths.sort_by(|a, b| basename(a).cmp(basename(b)));
    for rel in paths {
        note_row(ui, ctx, &rel, 0);
    }
}

fn render_by_folder(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>, paths: Vec<String>) {
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
fn note_row(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>, rel: &str, depth: usize) {
    let indent = 8.0 + depth as f32 * 14.0;
    ui.horizontal(|ui| {
        ui.add_space(indent);
        let resp = ui
            .add(
                egui::Button::image_and_text(
                    icons::ICONS.image(icons::Icon::File),
                    basename(rel),
                )
                .frame(false)
                .sense(egui::Sense::click_and_drag()),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        crate::widgets::note_row::note_drag_source(ui, &resp, rel, basename(rel));
        // The rich markdown hover preview is this row's single hover affordance —
        // no separate path tooltip, so the two don't overlap.
        if resp.hovered() {
            crate::widgets::preview::register_note_hover(ui, resp.rect, rel);
        }
        crate::item_menu::attach_note_item_menu(
            &resp,
            ctx,
            rel,
            crate::item_menu::BaseOpts { reveal: true },
        );
        if resp.clicked() {
            let sticky = crate::widgets::note_row::open_sticky(ui.input(|i| i.modifiers));
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

// ---- Activity impl ----------------------------------------------------

/// Zero-sized `Activity` descriptor for the Vault lens. State lives in
/// `AppState::vault_state`; the surface reaches it via
/// `ctx.state.downcast_mut::<State>()`.
pub struct Vault;

impl Activity<dyn AppCtx> for Vault {
    fn id(&self) -> &'static str {
        "vault"
    }
    fn label(&self) -> &'static str {
        "Vault"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::Vault)
    }
    fn views(&self) -> Vec<&dyn View<dyn AppCtx>> {
        vec![&VaultSidebar]
    }
}

struct VaultSidebar;

impl View<dyn AppCtx> for VaultSidebar {
    fn id(&self) -> &'static str {
        "vault"
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut (dyn AppCtx + 'static)) {
        let Some(mut ctx) = ctx.surface_ctx(self.state_key()) else {
            return;
        };
        let ctx = &mut ctx;
        egui::ScrollArea::vertical()
            .id_salt("panel-vault-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                render_body(ui, ctx);
            });
    }
}
