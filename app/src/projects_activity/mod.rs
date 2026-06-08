//! Projects activity — a sidebar `Activity` listing every **project note** (`hiker.kind: project`)
//! in the vault. A row click opens that project's code-entity graph (`panels::code_graph::open` —
//! the same opener the file tree uses for a project note); a per-row ⚙ opens the project-config
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
            value: "project".to_string(),
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

    let mut to_open: Option<String> = None;
    let mut to_edit: Option<String> = None;
    for (rel, title) in &projects {
        ui.horizontal(|ui| {
            let resp = ui
                .add(
                    egui::Label::new(egui::RichText::new(title).small())
                        .sense(egui::Sense::click()),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(format!("{rel}\nClick to open the code graph"));
            if resp.clicked() {
                to_open = Some(rel.clone());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("⚙").on_hover_text("Edit project config").clicked() {
                    to_edit = Some(rel.clone());
                }
            });
        });
    }

    if let Some(rel) = to_open {
        ctx.defer(move |app| {
            crate::panels::code_graph::open(app, &rel);
        });
    }
    if let Some(rel) = to_edit {
        ctx.defer(move |app| {
            crate::panels::project_config::open(app, Some(rel));
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
