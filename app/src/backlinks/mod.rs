//! Backlinks feature — sidebar surface listing notes whose `[[wikilink]]`
//! resolves to the active note. Migrated off `panels::backlinks` +
//! `panels_registry` to a real `Feature` rendering through the narrow
//! `feature::Ctx` (reads via ctx; open-note deferred via `ctx.defer`).
//! [feature-backlinks-migration]
//!
//! The backlinks set is a vault-wide content scan (no structural-index
//! API yet), cached by active path so it only re-runs on note switch.
//! status: wikilink-backlinks

use std::sync::Arc;

use eframe::egui;
use hiker_core::vault::Vault;

use crate::editor_pane;
use crate::feature::{Ctx, Feature, SidebarSurface};
use crate::icons;
use crate::theme;

/// Per-feature UI state for the backlinks surface — a cached vault-wide
/// wikilink scan, recomputed when the active note changes. Owned by
/// `AppState::backlinks_state` (top-level, per `feature-state-ownership`).
/// Inlined here rather than a sibling `state.rs` (too small to justify
/// its own file under `scripts/check-splits.py`).
#[derive(Default)]
pub struct State {
    /// Cached source paths that wikilink to `backlinks_for`.
    pub backlinks: Vec<String>,
    /// The note the cache was computed for (vault-relative path).
    pub backlinks_for: Option<String>,
}

/// Zero-sized `Feature` descriptor for backlinks. State lives in
/// `AppState::backlinks_state`; the surface reaches it via
/// `Ctx::state.downcast_mut::<State>()`.
pub struct Backlinks;

impl Feature for Backlinks {
    fn id(&self) -> &'static str {
        "backlinks"
    }
    fn label(&self) -> &'static str {
        "Backlinks"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::Bookmark)
    }
    fn sidebar(&self) -> Option<&dyn SidebarSurface> {
        Some(&BacklinksSidebar)
    }
}

struct BacklinksSidebar;

impl SidebarSurface for BacklinksSidebar {
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
        // The workbench accordion owns the section header + collapse;
        // the body is just the content. [feature-panel-single-accordion]
        ui.add_space(8.0);
        render_body(ui, ctx);
    }
}

fn render_body(ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
    let Some(active) = ctx.active_path.clone() else {
        ui.label(
            egui::RichText::new("(open a note to see backlinks)")
                .color(theme::muted())
                .small(),
        );
        return;
    };

    // Re-scan only when the active path changed since the last cache.
    let stale = ctx
        .state
        .downcast_ref::<State>()
        .expect("backlinks state")
        .backlinks_for
        .as_deref()
        != Some(active.as_str());
    if stale {
        let found = scan_backlinks(ctx.vault, ctx.config, &active);
        let st = ctx
            .state
            .downcast_mut::<State>()
            .expect("backlinks state");
        st.backlinks = found;
        st.backlinks_for = Some(active);
    }

    let hits = ctx
        .state
        .downcast_ref::<State>()
        .expect("backlinks state")
        .backlinks
        .clone();
    if hits.is_empty() {
        ui.label(
            egui::RichText::new("(no backlinks)")
                .color(theme::muted())
                .small(),
        );
        return;
    }
    for rel in &hits {
        let label = rel.rsplit('/').next().unwrap_or(rel.as_str());
        if ui
            .add(egui::Button::image_and_text(
                icons::ICONS.image(icons::Icon::File),
                label,
            ))
            .on_hover_text(rel)
            .clicked()
        {
            let rel_owned = rel.clone();
            ctx.defer(move |app| editor_pane::open_file(app, &rel_owned, false));
        }
    }
}

/// Scan every indexable note for a wikilink resolving to `active` under
/// the path-form (`wikilink-path-form`), honoring the configured
/// ambiguity policy. Returns deduped source paths.
fn scan_backlinks(
    vault: &Arc<Vault>,
    config: &Arc<std::sync::RwLock<hiker_core::config::Config>>,
    active: &str,
) -> Vec<String> {
    let Ok(paths) = vault.walk_indexable_files("") else {
        return Vec::new();
    };
    let policy = config
        .read()
        .map(|c| c.wikilinks.ambiguous_resolution.into())
        .unwrap_or(hiker_core::wikilink::AmbiguityPolicy::Unresolved);
    let mut out: Vec<String> = Vec::new();
    for rel in &paths {
        if rel == active {
            continue;
        }
        let Ok(body) = vault.read_file(rel) else {
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
