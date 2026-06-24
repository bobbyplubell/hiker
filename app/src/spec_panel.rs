//! Specs activity — a sidebar `Activity` that browses a project's specs and drives the center
//! code-entity graph. Pick a project; its specs are listed **nested by their defining doc and the
//! section heading they live under** (the way they're authored). Hovering a spec row lights its
//! governed footprint in the graph (a transient focus spotlight); clicking it opens that project's
//! code graph and selects the spec (locking the footprint). status: spec-activity-panel
//!
//! The cross-panel link is the standard deferred-effect pattern: a row hover/click queues a
//! `ctx.defer` closure that, with full `&mut AppState`, sets `code_graph_hover_spec` (hover) or
//! calls `code_graph::{open,select_spec}` (click). State (the selected project + the cached doc→
//! section→slug tree) lives on `AppState::specs_state`.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::activity::{AppCtx, SurfaceCtx};
use crate::tab::CodeSource;
use egui_workbench::activity::View;
use hiker_core::store::Store;
use hiker_core::vault::Vault;
use hiker_projects::Project;
use hiker_theme as theme;

/// Per-activity UI state: the picked project + the doc→section→slug tree cached for it. Owned by
/// `AppState::specs_state`.
#[derive(Default)]
pub struct State {
    /// The picked project note (vault-relative path) whose specs are shown + driven.
    selected_project: Option<String>,
    /// The cached nested spec tree for `selected_project` (rebuilt when the project changes).
    tree: Vec<DocGroup>,
    /// Which project `tree` was built for (cache key).
    tree_for: Option<String>,
    /// Cached project list (so the panel doesn't re-query the store every frame it's open). `None`
    /// until first built; rebuilt on a fresh `State` (e.g. vault reload). status: spec-activity-panel
    projects: Option<Vec<(String, String)>>,
}

/// One defining doc's specs, grouped by the section heading they sit under.
struct DocGroup {
    /// Vault-relative doc path (the `CollapsingHeader`'s stable id).
    doc: String,
    /// Display title (the doc basename).
    title: String,
    sections: Vec<Section>,
}

/// The specs under one `##`-style section heading within a doc.
struct Section {
    /// The heading text (empty for slugs before the first heading).
    heading: String,
    slugs: Vec<String>,
}

fn basename(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).trim_end_matches(".md").to_string()
}

/// The vault-relative `…/docs` prefix of `note`'s repo, or `None` if the note isn't a parseable
/// project (then the tree falls back to every spec doc in the vault).
fn project_docs_prefix(vault: &Vault, note: &str) -> Option<String> {
    let text = vault.read_file(note).ok()?;
    let project = Project::parse(&text, Path::new(note)).ok()?;
    let repo = project.repo_sources().next()?;
    let vault_root = vault.root();
    let root_abs = crate::panels::code_graph::resolve_in_vault(vault_root, &repo.root);
    let root_rel = root_abs.strip_prefix(vault_root).ok()?;
    Some(root_rel.join("docs").to_string_lossy().replace('\\', "/"))
}

/// Walk a doc body into `(section heading → slugs)` groups: track the current heading and attach
/// each `[slug]` anchor line to it. status: spec-activity-panel
fn parse_doc_sections(body: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut heading = String::new();
    let mut slugs: Vec<String> = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            if !slugs.is_empty() {
                sections.push(Section {
                    heading: std::mem::take(&mut heading),
                    slugs: std::mem::take(&mut slugs),
                });
            }
            heading = trimmed.trim_start_matches('#').trim().to_string();
        } else if let Some(slug) = hiker_code::governance::slug_in_line(line) {
            let slug = slug.to_string();
            if !slugs.contains(&slug) {
                slugs.push(slug);
            }
        }
    }
    if !slugs.is_empty() {
        sections.push(Section { heading, slugs });
    }
    sections
}

/// Every `hiker.kind: project` note as `(vault_rel_path, title)`, sorted by title — the picker's
/// options. Cached by the caller (a store query per frame is too costly while the panel is open).
fn query_projects(store: &Arc<Mutex<Store>>) -> Vec<(String, String)> {
    use hiker_core::store::dto::{MetaFilter, NoteQuery};
    let Ok(store) = store.lock() else { return Vec::new() };
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

/// Build the nested spec tree for one project: the spec docs under its `…/docs`, each parsed into
/// section→slug groups, sorted by title.
fn build_tree(store: &Arc<Mutex<Store>>, vault: &Vault, note: &str) -> Vec<DocGroup> {
    let prefix = project_docs_prefix(vault, note);
    let anchors = {
        let Ok(store) = store.lock() else { return Vec::new() };
        store.all_spec_anchors().unwrap_or_default()
    };
    let docs: BTreeSet<String> = anchors
        .into_iter()
        .filter(|(_, path)| prefix.as_ref().is_none_or(|pre| path.starts_with(pre)))
        .map(|(_, path)| path)
        .collect();
    let mut groups: Vec<DocGroup> = docs
        .into_iter()
        .filter_map(|doc| {
            let body = vault.read_file(&doc).ok()?;
            let sections = parse_doc_sections(&body);
            (!sections.is_empty()).then(|| DocGroup { title: basename(&doc), doc, sections })
        })
        .collect();
    groups.sort_by_key(|g| g.title.to_lowercase());
    groups
}

/// Render the Specs sidebar body through the narrow activity `SurfaceCtx`.
fn render_body(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>) {
    // Clone the shared handles so the `&mut state` section below touches no `ctx`.
    let store = Arc::clone(&ctx.services.read_store);
    let vault = Arc::clone(ctx.vault);

    // Highlight set (slugs to light), a click-to-select slug, and a doc to open — collected during
    // render, applied after the `&mut state` borrow ends.
    let mut hovered: Vec<String> = Vec::new();
    let mut clicked: Option<String> = None;
    let mut open_doc: Option<String> = None;
    let source: Option<CodeSource>;
    {
        let Some(state) = ctx.state.downcast_mut::<State>() else { return };
        // The project list is cached (a store query per frame is too costly while the panel is open);
        // clone it out for this frame's picker (cheap vs a query).
        if state.projects.is_none() {
            state.projects = Some(query_projects(&store));
        }
        let projects = state.projects.clone().unwrap_or_default();
        if projects.is_empty() {
            ui.label(
                egui::RichText::new("No projects in this vault").color(theme::muted()).small(),
            );
            return;
        }
        if state.selected_project.is_none() {
            state.selected_project = projects.first().map(|(p, _)| p.clone());
        }
        // Project picker.
        let current = state
            .selected_project
            .as_ref()
            .map(|p| basename(p))
            .unwrap_or_else(|| "Select project".to_string());
        egui::ComboBox::from_id_salt("specs-project").selected_text(current).show_ui(ui, |ui| {
            for (rel, title) in &projects {
                let sel = state.selected_project.as_deref() == Some(rel.as_str());
                if ui.selectable_label(sel, title).clicked() {
                    state.selected_project = Some(rel.clone());
                }
            }
        });
        ui.separator();

        // (Re)build the nested tree when the project changes.
        if state.tree_for != state.selected_project {
            state.tree = match &state.selected_project {
                Some(p) => build_tree(&store, &vault, p),
                None => Vec::new(),
            };
            state.tree_for = state.selected_project.clone();
        }

        if state.tree.is_empty() {
            ui.label(egui::RichText::new("No specs for this project").color(theme::muted()).small());
        }
        render_tree(ui, &state.tree, &mut hovered, &mut clicked, &mut open_doc);
        source = state.selected_project.clone().map(CodeSource::Project);
    }

    // Cross-panel effects (after the state borrow ends). Open-doc is project-independent; click
    // opens + selects; hover lights the (possibly many) footprints.
    if let Some(doc) = open_doc {
        ctx.defer(move |app| crate::editor_pane::open_file(app, &doc, false));
        return;
    }
    let Some(source) = source else { return };
    let key = source.key();
    if let Some(slug) = clicked {
        ctx.defer(move |app| {
            crate::panels::code_graph::open(app, source.clone());
            crate::panels::code_graph::select_spec(app, &source.key(), &slug);
        });
    } else if !hovered.is_empty() {
        ctx.defer(move |app| {
            app.panels.code_graph_hover_spec = Some((key, hovered));
        });
    }
}

/// Render the nested doc → section → slug tree (both levels collapsible), collecting the hovered
/// slug set, a click-to-select slug, and a right-click "open doc" target. Hovering a doc/section
/// header lights ALL specs beneath it; every level's right-click opens the defining doc.
fn render_tree(
    ui: &mut egui::Ui,
    tree: &[DocGroup],
    hov: &mut Vec<String>,
    clk: &mut Option<String>,
    opn: &mut Option<String>,
) {
    for group in tree {
        let header = egui::CollapsingHeader::new(&group.title).id_salt(&group.doc).show(ui, |ui| {
            for (si, section) in group.sections.iter().enumerate() {
                if section.heading.is_empty() {
                    render_slug_rows(ui, &section.slugs, &group.doc, hov, clk, opn);
                } else {
                    let sec = egui::CollapsingHeader::new(&section.heading)
                        .id_salt((group.doc.as_str(), si))
                        .show(ui, |ui| {
                            render_slug_rows(ui, &section.slugs, &group.doc, hov, clk, opn);
                        });
                    if sec.header_response.hovered() {
                        *hov = section.slugs.clone();
                    }
                    open_doc_menu(&sec.header_response, &group.doc, opn);
                }
            }
        });
        if header.header_response.hovered() {
            *hov = group.sections.iter().flat_map(|s| s.slugs.iter().cloned()).collect();
        }
        open_doc_menu(&header.header_response, &group.doc, opn);
    }
}

/// A column of clickable spec-slug rows: hover lights that one spec, click selects it, right-click
/// opens the defining doc.
fn render_slug_rows(
    ui: &mut egui::Ui,
    slugs: &[String],
    doc: &str,
    hov: &mut Vec<String>,
    clk: &mut Option<String>,
    opn: &mut Option<String>,
) {
    for slug in slugs {
        let resp = ui
            .add(egui::Label::new(egui::RichText::new(slug).small()).sense(egui::Sense::click()))
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("Hover to highlight · click to select · right-click to open the doc");
        if resp.hovered() {
            *hov = vec![slug.clone()];
        }
        if resp.clicked() {
            *clk = Some(slug.clone());
        }
        open_doc_menu(&resp, doc, opn);
    }
}

/// Attach an "Open source document" right-click menu to `resp`, recording the doc to open.
fn open_doc_menu(resp: &egui::Response, doc: &str, opn: &mut Option<String>) {
    resp.context_menu(|ui| {
        if ui.button("Open source document").clicked() {
            *opn = Some(doc.to_string());
            ui.close();
        }
    });
}

// ---- View impl --------------------------------------------------------

/// The Specs sidebar view. Not its own activity — it's a second view under the **Projects** activity
/// (`projects_activity`), so it shows as a "Specs" section beneath the project list rather than a
/// standalone activity-bar icon. status: spec-activity-panel
pub(crate) struct SpecsListView;

impl View<dyn AppCtx> for SpecsListView {
    fn id(&self) -> &'static str {
        "specs"
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut (dyn AppCtx + 'static)) {
        let Some(mut ctx) = ctx.surface_ctx(self.state_key()) else {
            return;
        };
        let ctx = &mut ctx;
        egui::ScrollArea::vertical()
            .id_salt("panel-specs-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                render_body(ui, ctx);
            });
    }
}

#[cfg(test)]
mod tests {
    use super::parse_doc_sections;

    /// Sections are split by headings; each `[slug]` anchor attaches to the heading above it; lines
    /// before the first heading form an empty-heading group; non-anchor brackets are ignored.
    #[test]
    fn parse_groups_slugs_under_their_headings() {
        let body = "\
# Title
intro with a [pre-heading-slug] anchor

## First section
a line [slug-one] here
another [slug-two]

## Empty section
no anchors here, just a [[spec:link]]

## Third
final [slug-three]
";
        let sections = parse_doc_sections(body);
        let by_heading: Vec<(&str, Vec<&str>)> = sections
            .iter()
            .map(|s| (s.heading.as_str(), s.slugs.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            by_heading,
            vec![
                ("Title", vec!["pre-heading-slug"]),
                ("First section", vec!["slug-one", "slug-two"]),
                ("Third", vec!["slug-three"]),
            ]
        );
    }
}
