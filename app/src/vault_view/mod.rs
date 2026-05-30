//! Vault view — a read-only *logical lens* over the vault's notes,
//! offered as a sidebar mode alongside the literal on-disk Files tree.
//! See `docs/vault-view.md`.
//!
//! Files mode shows where bytes live (the real, nested directory tree);
//! Vault mode shows how the knowledge is organized — derived from the
//! index + frontmatter, never moving a file. v1 ships the grouping
//! that's derivable from data that exists today (by top-level folder,
//! flattened; or flat-by-name). The richer groupings the spec describes
//! — crawl-job nesting (`vault-view-crawl-nesting`), sidecar surfacing
//! (`vault-view-sidecar-surfacing`), source-type / provenance groups
//! (`vault-view-source-groups`) — light up when `extract.md` + a
//! provenance index column land; the lens dispatch below has a slot for
//! each.
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

/// Which derived grouping the Vault lens renders. Display state only —
/// never stored on a note (`vault-view-readonly-lens`). Selectable from
/// the mode's `⋯` menu.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Lens {
    /// Notes grouped under a virtual node per top-level folder, flattened
    /// within each (distinct from Files' fully-nested tree). Default.
    #[default]
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
            .color(crate::theme::muted())
            .small(),
    );
    let cur = app.vault_state.lens;
    for (label, lens) in [("Folder", Lens::ByFolder), ("Flat (all notes)", Lens::Flat)] {
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
