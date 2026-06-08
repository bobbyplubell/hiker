//! Project-config tab (`TabKind::ProjectConfig`): author/edit a project note (`hiker.kind: project`)
//! through a **UI form** instead of hand-writing YAML frontmatter. The form collects a project name
//! and a list of external **sources** (repo / docs), each with its kind-specific fields, then
//! **Save** serializes them to a project note (nested `hiker: { kind: project }` frontmatter — the
//! form hiker's own notes use and `hiker-projects` parses) and opens the project's code graph.
//!
//! Per-tab form state lives on `AppState::panels.project_config`, keyed by tab id.

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::tab::{Tab, TabId, TabKind};
use hiker_core::indexer::IndexJob;
use hiker_theme as theme;

/// Which kind of external source a row configures. (Only the kinds with a working binding are
/// offered: `repo` → SCIP code graph, `docs` → a content folder. Jira/LSP are design-level future
/// sources with no adapter yet, so they're intentionally not selectable here.)
#[derive(Clone, Copy, PartialEq, Eq)]
enum SrcKind {
    Repo,
    Docs,
}

impl SrcKind {
    fn label(self) -> &'static str {
        match self {
            SrcKind::Repo => "repo (code)",
            SrcKind::Docs => "docs",
        }
    }
    fn wire(self) -> &'static str {
        match self {
            SrcKind::Repo => "repo",
            SrcKind::Docs => "docs",
        }
    }
}

/// One source row's editable fields (superset across kinds; only the relevant ones render/save).
#[derive(Default)]
struct SourceForm {
    root: String,
    repo_id: String,
    index: String,
    include: String, // one glob per line
    exclude: String,
}

/// Per-tab project-config form state.
#[derive(Default)]
pub struct ProjectConfigForm {
    loaded: bool,
    name: String,
    kinds: Vec<SrcKind>,
    sources: Vec<SourceForm>,
    saved_as: Option<String>,
    status: Option<(String, bool)>, // (message, is_error)
}

/// Find-or-focus a project-config tab; `source_note` = `Some(path)` to edit an existing project
/// note, `None` for a fresh one.
pub fn open(app: &mut AppState, source_note: Option<String>) -> TabId {
    if let Some(existing) = app.session.tabs.iter().find(
        |t| matches!(&t.kind, TabKind::ProjectConfig { source_note: s } if *s == source_note),
    ) {
        let id = existing.id;
        app.session.active_tab = Some(id);
        return id;
    }
    let id = app.next_tab_id();
    app.session.tabs.push(Tab::new(id, TabKind::ProjectConfig { source_note }, true));
    app.session.active_tab = Some(id);
    id
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId, source_note: Option<&str>) {
    if !app.panels.project_config.get(&tab_id).is_some_and(|f| f.loaded) {
        let form = build_initial(app, source_note);
        app.panels.project_config.insert(tab_id, form);
    }
    let editing = source_note.is_some();
    ui.heading(if editing { "Edit project" } else { "New project" });
    ui.label(
        egui::RichText::new("Configure external sources, then Save to write a project note.")
            .color(theme::muted())
            .small(),
    );
    ui.separator();

    let mut do_save = false;
    if let Some(form) = app.panels.project_config.get_mut(&tab_id) {
        render_form(ui, form, &mut do_save);
    }
    if do_save {
        let result = save_project(app, tab_id, source_note);
        if let Some(form) = app.panels.project_config.get_mut(&tab_id) {
            match &result {
                Ok(rel) => {
                    form.status = Some((format!("Saved → {rel}"), false));
                    form.saved_as = Some(rel.clone());
                }
                Err(e) => form.status = Some((e.clone(), true)),
            }
        }
        if let Ok(rel) = result {
            crate::panels::code_graph::open(app, &rel);
        }
    }

    if let Some(form) = app.panels.project_config.get(&tab_id) {
        if let Some((msg, is_err)) = &form.status {
            let color = if *is_err { egui::Color32::from_rgb(200, 60, 60) } else { theme::accent() };
            ui.add_space(4.0);
            ui.colored_label(color, msg);
        }
    }
}

/// Build the initial form: load an existing note's frontmatter into fields, or seed a blank form
/// with one repo source for a new project.
fn build_initial(app: &AppState, source_note: Option<&str>) -> ProjectConfigForm {
    let mut form = ProjectConfigForm { loaded: true, ..Default::default() };
    if let Some(path) = source_note {
        form.name = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".md").to_string();
        if let Ok(text) = app.vault_session.vault.read_file(path) {
            load_sources_into(&mut form, &text);
        }
    }
    if form.sources.is_empty() {
        form.kinds.push(SrcKind::Repo);
        form.sources.push(SourceForm::default());
    }
    form
}

/// Parse a project note's raw frontmatter `sources[]` into the form rows (faithful round-trip — no
/// path expansion / id derivation, unlike a bound `hiker_projects::Project`).
fn load_sources_into(form: &mut ProjectConfigForm, text: &str) {
    let Some(fm) = hiker_core::frontmatter::split(text).frontmatter else { return };
    let Ok(raw) = serde_yml::from_value::<RawNote>(fm) else { return };
    for s in raw.sources.unwrap_or_default() {
        // Unknown / not-yet-supported kinds (e.g. a hand-authored `jira`) load as a `docs` row so
        // the path is preserved and visible rather than silently dropped.
        let kind = if s.kind == "repo" { SrcKind::Repo } else { SrcKind::Docs };
        form.kinds.push(kind);
        form.sources.push(SourceForm {
            root: s.root.unwrap_or_default(),
            repo_id: s.repo_id.unwrap_or_default(),
            index: s.index.unwrap_or_default(),
            include: s.scope.as_ref().map(|c| c.include.join("\n")).unwrap_or_default(),
            exclude: s.scope.as_ref().map(|c| c.exclude.join("\n")).unwrap_or_default(),
        });
    }
}

/// Render the editable form; sets `*do_save` when the Save button is clicked.
fn render_form(ui: &mut egui::Ui, form: &mut ProjectConfigForm, do_save: &mut bool) {
    ui.horizontal(|ui| {
        ui.label("Project name:");
        ui.text_edit_singleline(&mut form.name);
    });
    ui.add_space(6.0);
    ui.label(egui::RichText::new("Sources").strong());

    let mut remove: Option<usize> = None;
    for i in 0..form.sources.len() {
        render_source(ui, i, form, &mut remove);
    }
    if let Some(i) = remove {
        form.sources.remove(i);
        form.kinds.remove(i);
    }

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.button("+ Add source").clicked() {
            form.kinds.push(SrcKind::Repo);
            form.sources.push(SourceForm::default());
        }
        if ui.add(egui::Button::new("💾 Save project")).clicked() {
            *do_save = true;
        }
    });
}

/// Render one source row (kind selector + kind-specific fields + remove).
fn render_source(ui: &mut egui::Ui, i: usize, form: &mut ProjectConfigForm, remove: &mut Option<usize>) {
    egui::Frame::default()
        .fill(theme::active_bg())
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt(("src-kind", i))
                    .selected_text(form.kinds[i].label())
                    .show_ui(ui, |ui| {
                        for k in [SrcKind::Repo, SrcKind::Docs] {
                            ui.selectable_value(&mut form.kinds[i], k, k.label());
                        }
                    });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✕").on_hover_text("Remove source").clicked() {
                        *remove = Some(i);
                    }
                });
            });
            let s = &mut form.sources[i];
            match form.kinds[i] {
                SrcKind::Repo => render_repo_fields(ui, i, s),
                SrcKind::Docs => field(ui, "root (folder)", &mut s.root),
            }
        });
    ui.add_space(4.0);
}

fn render_repo_fields(ui: &mut egui::Ui, i: usize, s: &mut SourceForm) {
    field(ui, "root (repo dir)", &mut s.root);
    field(ui, "index (.scip path)", &mut s.index);
    field(ui, "repo_id (optional)", &mut s.repo_id);
    ui.horizontal_top(|ui| {
        ui.label("scope include:");
        ui.add(egui::TextEdit::multiline(&mut s.include).id_salt(("inc", i)).desired_rows(2).hint_text("src/**"));
        ui.label("exclude:");
        ui.add(egui::TextEdit::multiline(&mut s.exclude).id_salt(("exc", i)).desired_rows(2).hint_text("target/**"));
    });
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::TextEdit::singleline(value).desired_width(320.0));
    });
}

/// Serialize the form to a project note and write it to the vault (overwrite for an edit, else a new
/// `projects/<slug>.md`). Returns the vault-relative path on success.
fn save_project(app: &mut AppState, tab_id: TabId, source_note: Option<&str>) -> Result<String, String> {
    let form = app.panels.project_config.get(&tab_id).ok_or("form gone")?;
    if form.name.trim().is_empty() {
        return Err("project name is required".to_string());
    }
    let content = render_note(form)?;
    let rel = match source_note {
        Some(p) => p.to_string(),
        None => format!("projects/{}.md", slugify(&form.name)),
    };
    let watcher = app.vault_session.services.watcher.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    watcher.suppress(rel.clone());
    vault.write_file(&rel, &content).map_err(|e| format!("write {rel}: {e}"))?;
    watcher.suppress(rel.clone());
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let rel2 = rel.clone();
        handle.block_on(async move {
            let _ = jobs.send(IndexJob::Upsert { rel_path: rel2, force: false }).await;
        });
    }
    app.file_tree_state.invalidate_all();
    Ok(rel)
}

/// Build the project-note markdown: nested `hiker: { kind: project }` frontmatter + `sources[]` + a
/// short body.
fn render_note(form: &ProjectConfigForm) -> Result<String, String> {
    let sources: Vec<SrcOut> = form
        .sources
        .iter()
        .enumerate()
        .map(|(i, s)| source_out(form.kinds[i], s))
        .collect();
    let file = NoteFile { hiker: HikerKind { kind: "project" }, sources };
    let body = format!("# {}\n\nProject note (configured via the Projects UI).\n", form.name);
    let yaml = serde_yml::to_value(&file).map_err(|e| e.to_string())?;
    hiker_core::frontmatter::assemble(&yaml, &body).map_err(|e| e.to_string())
}

fn lines(s: &str) -> Vec<String> {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Map one form row to its serialized source, including only the fields relevant to its kind.
fn source_out(kind: SrcKind, s: &SourceForm) -> SrcOut {
    let mut out = SrcOut { kind: kind.wire().to_string(), ..Default::default() };
    match kind {
        SrcKind::Repo => {
            out.root = opt(&s.root);
            out.index = opt(&s.index);
            out.repo_id = opt(&s.repo_id);
            out.backend = Some("scip".to_string()); // the only implemented backend
            let (inc, exc) = (lines(&s.include), lines(&s.exclude));
            if !inc.is_empty() || !exc.is_empty() {
                out.scope = Some(ScopeOut { include: inc, exclude: exc });
            }
        }
        SrcKind::Docs => out.root = opt(&s.root),
    }
    out
}

/// Lowercase dash-separated filename slug from a project name.
fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

// ---- wire types (serialize on save / deserialize on load) ----

#[derive(Serialize)]
struct NoteFile {
    hiker: HikerKind,
    sources: Vec<SrcOut>,
}

#[derive(Serialize)]
struct HikerKind {
    kind: &'static str,
}

#[derive(Serialize, Default)]
struct SrcOut {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    index: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<ScopeOut>,
}

#[derive(Serialize)]
struct ScopeOut {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exclude: Vec<String>,
}

#[derive(Deserialize)]
struct RawNote {
    sources: Option<Vec<RawSrc>>,
}

#[derive(Deserialize, Default)]
struct RawSrc {
    kind: String,
    root: Option<String>,
    repo_id: Option<String>,
    index: Option<String>,
    scope: Option<RawScope>,
}

#[derive(Deserialize, Default)]
struct RawScope {
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A form-saved note must round-trip back through `hiker_projects::Project` — the contract that
    /// makes a UI-authored project usable by the code-graph view.
    #[test]
    fn saved_note_parses_back() {
        let form = ProjectConfigForm {
            loaded: true,
            name: "My Proj".to_string(),
            kinds: vec![SrcKind::Repo, SrcKind::Docs],
            sources: vec![
                SourceForm {
                    root: "/home/me/proj".to_string(),
                    index: "/home/me/proj.scip".to_string(),
                    repo_id: "rid".to_string(),
                    include: "src/**\nlib/**".to_string(),
                    exclude: "target/**".to_string(),
                    ..Default::default()
                },
                SourceForm { root: "/home/me/proj/docs".to_string(), ..Default::default() },
            ],
            ..Default::default()
        };
        let note = render_note(&form).expect("render");
        // Nested frontmatter convention (what hiker's own notes use).
        assert!(note.contains("hiker:"));
        assert!(note.contains("kind: project"));

        let project = hiker_projects::Project::parse(&note, std::path::Path::new("p.md"))
            .expect("parse back");
        let repo = project.repo_sources().next().expect("repo source");
        assert_eq!(repo.repo_id, "rid");
        assert_eq!(repo.index, std::path::PathBuf::from("/home/me/proj.scip"));
        assert!(repo.scope.accepts("src/main.rs"));
        assert!(!repo.scope.accepts("target/x.rs"));
        // The docs source survives as a recognized (unsupported-for-binding) source.
        assert_eq!(project.sources.len(), 2);
    }

    #[test]
    fn slugify_basics() {
        assert_eq!(slugify("My Proj!"), "my-proj");
        assert_eq!(slugify("a__b"), "a-b");
    }
}
