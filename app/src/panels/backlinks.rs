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

pub struct State {
    /// Cached backlinks for the note in `backlinks_for`. Recomputed when
    /// the active note changes.
    pub backlinks: Vec<String>,
    pub backlinks_for: Option<String>,
    pub backlinks_expanded: bool,
}

impl State {
    pub const fn with_config(self, _cfg: &hiker_core::config::Config) -> Self {
        self
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            backlinks: Vec::new(),
            backlinks_for: None,
            backlinks_expanded: true,
        }
    }
}

/// Per-frame render context for the backlinks sub-panel. Bundling
/// `ui` + `app` lets the scan/render steps be inherent methods.
pub(crate) struct View<'a> {
    pub(crate) ui: &'a mut egui::Ui,
    pub(crate) app: &'a mut AppState,
}

impl View<'_> {
    pub(crate) fn show(&mut self) {
        // The workbench accordion provides the section header + collapse;
        // the panel body is just the content. [feature-panel-single-accordion]
        self.ui.add_space(8.0);
        self.backlinks_for_active();
    }

    fn backlinks_for_active(&mut self) {
        let active_path = self.app.session.active_tab.and_then(|id| {
            self.app
                .tab_by_id(id)
                .and_then(|t| t.buffer_path().map(str::to_string))
        });
        let Some(active) = active_path else {
            self.ui.label(
                egui::RichText::new("(open a note to see backlinks)")
                    .color(theme::muted())
                    .small(),
            );
            return;
        };

        // Cache: only rescan when the active path changes. The scan walks
        // all markdown files in the vault — fine for thousands of files,
        // but we don't want to redo it every frame.
        if self.app.panels.backlinks.backlinks_for.as_deref() != Some(active.as_str()) {
            let found = self.scan_backlinks(&active);
            self.app.panels.backlinks.backlinks = found;
            self.app.panels.backlinks.backlinks_for = Some(active.clone());
        }

        if self.app.panels.backlinks.backlinks.is_empty() {
            self.ui.label(
                egui::RichText::new("(no backlinks)")
                    .color(theme::muted())
                    .small(),
            );
            return;
        }
        let hits = self.app.panels.backlinks.backlinks.clone();
        for rel in &hits {
            let label = rel.rsplit('/').next().unwrap_or(rel.as_str());
            if self
                .ui
                .add(egui::Button::image_and_text(
                    crate::icons::ICONS.image(crate::icons::Icon::File),
                    label,
                ))
                .on_hover_text(rel)
                .clicked()
            {
                editor_pane::open_file(self.app, rel, /* sticky */ false);
            }
        }
    }

    /// Scan every indexable note in the vault for wikilinks resolving to
    /// `active` under the path-form (`wikilink-path-form`). Each candidate
    /// link is resolved through `wikilink::resolve_path` against the live
    /// path set, so we honor the same ambiguity policy as render and click.
    /// Returns a deduped list of source paths.
    ///
    /// status: wikilink-backlinks
    fn scan_backlinks(&self, active: &str) -> Vec<String> {
        let Ok(paths) = self.app.vault_session.vault.walk_indexable_files("") else {
            return Vec::new();
        };
        let policy = self
            .app
            .vault_session
            .config
            .read()
            .map(|c| c.wikilinks.ambiguous_resolution.into())
            .unwrap_or(hiker_core::wikilink::AmbiguityPolicy::Unresolved);
        let mut out: Vec<String> = Vec::new();
        for rel in &paths {
            if rel == active {
                continue;
            }
            let Ok(body) = self.app.vault_session.vault.read_file(rel) else {
                continue;
            };
            let links_here = hiker_core::wikilink::parse_links(&body).into_iter().any(|l| {
                matches!(
                    hiker_core::wikilink::resolve_path(&paths, &l.target, policy, Some(rel)),
                    hiker_core::wikilink::Resolution::Resolved(p) if p == active,
                )
            });
            if links_here {
                out.push(rel.clone());
            }
        }
        out
    }
}
