//! Projects activity — a sidebar `Activity` listing every **project note** (`hiker.kind: project`)
//! in the vault. A row click opens that project's code-entity graph (`panels::code_graph::open` —
//! the same opener the file tree uses for a project note); a per-row Edit button opens the project-config
//! form to edit it. A **"+ New project"** button opens a fresh config form
//! (`panels::project_config`) to author a new project note via UI instead of hand-writing YAML.
//!
//! The listing is read fresh each frame from the store's frontmatter index, so the activity carries
//! no real state; the zero-field [`State`] marker keeps the registry's `surface_ctx` seam uniform
//! (owned by `AppState::projects_activity_state`).

use eframe::egui;

use crate::activity::{AppCtx, SurfaceCtx};
use crate::icons;
use egui_workbench::activity::{Activity, View};
use hiker_core::store::dto::{MetaFilter, NoteQuery};
use hiker_theme as theme;

/// Per-activity UI state marker (the listing is read fresh each frame). Owned by
/// `AppState::projects_activity_state`.
#[derive(Default)]
pub struct State;

/// A verb on a project row's right-click menu (`interaction.md`
/// [rightclick-menu-always]): a project row names a note (`hiker.kind:
/// project`), so it carries the shared note-item base plus the row's own
/// verbs.
#[derive(Clone, Copy, Debug)]
enum RowVerb {
    /// A shared note-item base verb (Open / Reveal / Properties).
    Base(crate::item_menu::ItemAction),
    /// Open the project's code-entity graph (the row click's verb).
    OpenGraph,
    /// Open the project-config form on this note (the Edit button's verb).
    Configure,
}

/// Build a project row's context menu: the shared note-item base, then the
/// project-contextual section (Open code graph / Edit project config).
fn build_project_row_menu(rel: &str) -> egui_workbench::menu::Menu<RowVerb> {
    use crate::item_menu::{note_item_base, BaseOpts};
    note_item_base(rel, BaseOpts { reveal: true }, RowVerb::Base)
        .section()
        .action("Open code graph", RowVerb::OpenGraph)
        .action("Edit project config", RowVerb::Configure)
}

fn basename(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).trim_end_matches(".md").to_string()
}

/// Every `hiker.kind: project` note as `(vault_rel_path, title)`, via the store's frontmatter index
/// (the same discovery mechanism cluster-presets use), sorted by title.
pub fn list_projects(ctx: &SurfaceCtx<'_>) -> Vec<(String, String)> {
    let Ok(store) = ctx.services.read_store.lock() else { return Vec::new() };
    let query = NoteQuery {
        filters: vec![MetaFilter::Equals {
            key: "hiker.kind".to_string(),
            values: vec!["project".to_string()],
        }],
        ..Default::default()
    };
    let mut rows: Vec<(String, String)> = store
        .query_notes(&query)
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let title = if r.title.is_empty() { basename(&r.path) } else { r.title };
            (r.path, title)
        })
        .collect();
    rows.sort_by_key(|r| r.1.to_lowercase());
    rows
}

/// Render the Projects sidebar body through the narrow activity `SurfaceCtx`.
fn render_body(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>) {
    if ui
        .button("+ New project")
        .on_hover_text("Configure a new project (sources → save as a note)")
        .clicked()
    {
        ctx.defer(|app| {
            crate::panels::project_config::open(app, None);
        });
    }
    ui.add_space(6.0);

    let projects = list_projects(ctx);
    if projects.is_empty() {
        ui.label(
            egui::RichText::new("No projects in this vault").color(theme::muted()).small(),
        );
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new("Create one with “+ New project”.").color(theme::muted()).small(),
        );
        return;
    }

    let mut picked: Option<(String, RowVerb)> = None;
    for (rel, title) in &projects {
        let row = ui.horizontal(|ui| {
            let resp = ui
                .add(
                    egui::Label::new(egui::RichText::new(title).small())
                        .sense(egui::Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(format!("{rel}\nClick to open the code graph"));
            if resp.clicked() {
                picked = Some((rel.clone(), RowVerb::OpenGraph));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Edit").on_hover_text("Edit project config").clicked() {
                    picked = Some((rel.clone(), RowVerb::Configure));
                }
            });
        });
        // Right-click anywhere on the row → its context menu (`interaction.md`
        // [rightclick-menu-always]): note base + Open code graph / Configure.
        let mut chosen = None;
        row.response.interact(egui::Sense::click()).context_menu(|ui| {
            chosen = egui_workbench::menu::show(ui, build_project_row_menu(rel));
        });
        if let Some(verb) = chosen {
            picked = Some((rel.clone(), verb));
        }
    }

    if let Some((rel, verb)) = picked {
        ctx.defer(move |app| match verb {
            RowVerb::Base(action) => crate::item_menu::apply_item_action(app, action, &rel),
            RowVerb::OpenGraph => {
                crate::panels::code_graph::open(app, crate::tab::CodeSource::Project(rel));
            }
            RowVerb::Configure => {
                crate::panels::project_config::open(app, Some(rel));
            }
        });
    }
}

// ---- Activity impl ----------------------------------------------------

/// Zero-sized `Activity` descriptor for the Projects panel.
pub struct ProjectsActivity;

impl Activity<dyn AppCtx> for ProjectsActivity {
    fn id(&self) -> &'static str {
        "projects"
    }
    fn label(&self) -> &'static str {
        "Projects"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::Braces)
    }
    fn views(&self) -> Vec<&dyn View<dyn AppCtx>> {
        vec![&ProjectsListView]
    }
}

struct ProjectsListView;

impl View<dyn AppCtx> for ProjectsListView {
    fn id(&self) -> &'static str {
        "projects"
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut (dyn AppCtx + 'static)) {
        let Some(mut ctx) = ctx.surface_ctx(self.state_key()) else {
            return;
        };
        let ctx = &mut ctx;
        egui::ScrollArea::vertical()
            .id_salt("panel-projects-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                render_body(ui, ctx);
            });
    }
}

#[cfg(test)]
mod tests {
    use egui_workbench::menu::Entry;

    use super::{build_project_row_menu, RowVerb};

    /// Menu composition: the shared note-item base (a project row names a
    /// note), then the project-contextual section.
    #[test]
    fn project_row_menu_composes_base_plus_project_verbs() {
        let menu = build_project_row_menu("projects/x.md");
        let sections = menu.sections();
        assert_eq!(sections.len(), 2, "note base section + project section");
        assert_eq!(sections[0].len(), 5, "Open · Reveal · Open in graph · Copy path · Properties");
        let labels: Vec<&str> = sections[1]
            .iter()
            .map(|e| match e {
                Entry::Action { label, .. } => label.as_ref(),
                _ => panic!("expected Action entries"),
            })
            .collect();
        assert_eq!(labels, ["Open code graph", "Edit project config"]);
        assert!(matches!(
            sections[1][0],
            Entry::Action { action: RowVerb::OpenGraph, .. }
        ));
        assert!(matches!(
            sections[1][1],
            Entry::Action { action: RowVerb::Configure, .. }
        ));
    }
}
