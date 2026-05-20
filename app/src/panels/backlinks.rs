//! Backlinks sub-panel of the right-hand discovery pane.
//!
//! Lists notes containing a `[[wikilink]]` pointing at the active note.
//! Implemented as a vault-wide content scan (we don't yet have a
//! structural-index API). Costs roughly O(total bytes) per refresh;
//! results are cached keyed by the active path so the scan only re-runs
//! when the active note changes.

use eframe::egui;

use crate::editor_pane;
use crate::state::AppState;
use crate::theme;

pub struct BacklinksState {
    /// Cached backlinks for the note in `backlinks_for`. Recomputed when
    /// the active note changes.
    pub backlinks: Vec<String>,
    pub backlinks_for: Option<String>,
    pub backlinks_expanded: bool,
}

impl BacklinksState {
    pub fn from_config(_cfg: &hiker_core::config::Config) -> Self {
        Self::default()
    }
}

impl Default for BacklinksState {
    fn default() -> Self {
        Self {
            backlinks: Vec::new(),
            backlinks_for: None,
            backlinks_expanded: true,
        }
    }
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.add_space(12.0);
    let backlinks_expanded = app.panels.backlinks.backlinks_expanded;
    let backlinks_count = app.panels.backlinks.backlinks.len();
    if crate::panels::discovery_pane::collapsible_header(
        ui,
        "backlinks",
        "Backlinks",
        backlinks_expanded,
        backlinks_count,
    ) {
        app.panels.backlinks.backlinks_expanded = !backlinks_expanded;
        crate::panels::search::persist_search_setting(
            app,
            "search.sections.backlinks_expanded",
            serde_json::json!(app.panels.backlinks.backlinks_expanded),
        );
    }
    if app.panels.backlinks.backlinks_expanded {
        backlinks_for_active(ui, app);
    }
}

fn backlinks_for_active(ui: &mut egui::Ui, app: &mut AppState) {
    let active_path = app.session.active_tab.and_then(|id| {
        app.tab_by_id(id)
            .and_then(|t| t.buffer_path().map(str::to_string))
    });
    let Some(active) = active_path else {
        ui.label(
            egui::RichText::new("(open a note to see backlinks)")
                .color(theme::muted())
                .small(),
        );
        return;
    };

    // Cache: only rescan when the active path changes. The scan walks all
    // markdown files in the vault — fine for thousands of files, but we
    // don't want to redo it every frame.
    if app.panels.backlinks.backlinks_for.as_deref() != Some(active.as_str()) {
        app.panels.backlinks.backlinks = scan_backlinks(app, &active);
        app.panels.backlinks.backlinks_for = Some(active.clone());
    }

    if app.panels.backlinks.backlinks.is_empty() {
        ui.label(
            egui::RichText::new("(no backlinks)")
                .color(theme::muted())
                .small(),
        );
        return;
    }
    let hits = app.panels.backlinks.backlinks.clone();
    for rel in &hits {
        let label = rel.rsplit('/').next().unwrap_or(rel.as_str());
        if ui
            .add(egui::Button::image_and_text(crate::icons::file(), label))
            .on_hover_text(rel)
            .clicked()
        {
            editor_pane::open_file(app, rel, /* sticky */ false);
        }
    }
}

/// Scan every indexable note in the vault for `[[Target]]` wikilinks whose
/// target resolves to `active`. Returns a deduped list of source paths.
/// Resolution is intentionally lenient: a wikilink matches if its target
/// equals `active`, `active` with `.md` stripped, the basename of `active`,
/// or the basename without extension.
fn scan_backlinks(app: &AppState, active: &str) -> Vec<String> {
    let targets = wikilink_target_aliases(active);
    let Ok(paths) = app.vault_session.vault.walk_indexable_files("") else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for rel in paths {
        if rel == active {
            continue;
        }
        let Ok(body) = app.vault_session.vault.read_file(&rel) else {
            continue;
        };
        if has_wikilink_to(&body, &targets) {
            out.push(rel);
        }
    }
    out
}

fn wikilink_target_aliases(rel: &str) -> Vec<String> {
    let mut v = Vec::new();
    v.push(rel.to_string());
    if let Some(stripped) = rel.strip_suffix(".md") {
        v.push(stripped.to_string());
    }
    let base = rel.rsplit('/').next().unwrap_or(rel);
    v.push(base.to_string());
    if let Some(stripped) = base.strip_suffix(".md") {
        v.push(stripped.to_string());
    }
    v
}

fn has_wikilink_to(body: &str, targets: &[String]) -> bool {
    // Cheap byte scan for `[[target]]` (with optional `|alias`). We avoid
    // a regex dependency in this hot path.
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            let inner_start = i + 2;
            let mut j = inner_start;
            while j + 1 < bytes.len()
                && !(bytes[j] == b']' && bytes[j + 1] == b']')
                && bytes[j] != b'\n'
            {
                j += 1;
            }
            if j + 1 < bytes.len() && bytes[j] == b']' && bytes[j + 1] == b']' {
                let target = &body[inner_start..j];
                let target = target.split('|').next().unwrap_or(target).trim();
                if !target.is_empty() && targets.iter().any(|t| t == target) {
                    return true;
                }
                i = j + 2;
                continue;
            }
        }
        i += 1;
    }
    false
}
