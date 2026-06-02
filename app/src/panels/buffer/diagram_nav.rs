//! Interactive mermaid diagram navigation for the buffer panel: turning a
//! clicked diagram `click`-directive region into an opened target, and showing
//! a hover tooltip over a region that carries one.
//!
//! Kept beside `mod.rs` (like `wikilink_nav`) so the editor render path stays
//! within its length budget. This is the app-layer seam between the
//! `widgets::MermaidWidget` interaction regions (which the painter turns into
//! `WidgetClick` zones tagged with `widgets::MERMAID_REGION_TAG`) and the
//! action handlers (`core::url::classify` → OS opener / ZIM viewer / note open).
//!
//! `dispatch_link` is the single place a classified [`LinkTarget`] maps to an
//! action, so the diagram path and any future link surface share one mapping.
//!
//! status: widget-mermaid-links, widget-diagram-hover-tooltip

use eframe::egui;
use editor_view::viewport::{ClickAction, ClickZone};
use hiker_core::url::{self, LinkTarget};
use hiker_core::wikilink::{self, AmbiguityPolicy, Resolution};

use super::widgets::DiagramRegionRegistry;
use crate::state::{AppState, ToastLevel};

/// Dispatch this frame's diagram-region clicks. Each tagged id is looked up in
/// the per-frame `registry`; its link string is classified and routed through
/// [`dispatch_link`]. status: widget-mermaid-links
pub(crate) fn handle_clicks(
    app: &mut AppState,
    ctx: &egui::Context,
    clicks: &[u64],
    registry: &DiagramRegionRegistry,
    sticky: bool,
) {
    if clicks.is_empty() {
        return;
    }
    let mut acted = false;
    for &id in clicks {
        let Some(entry) = registry.get(&id) else { continue };
        let Some(link) = entry.link.as_deref() else { continue };
        dispatch_link(app, &url::classify(link), sticky);
        acted = true;
    }
    if acted {
        ctx.request_repaint();
    }
}

/// The single `LinkTarget` → action mapping shared by every interactive link
/// surface. `sticky` (Mod-click) opens note targets in a sticky tab rather than
/// the preview slot. status: widget-mermaid-links
///
/// - [`External`](LinkTarget::External): hand to the OS default handler
///   (browser / mail client).
/// - [`Zim`](LinkTarget::Zim): resolve the archive authority to a vault `.zim`
///   path, then open the ZIM viewer at the article.
/// - [`VaultPath`](LinkTarget::VaultPath) / [`Wikilink`](LinkTarget::Wikilink):
///   resolve to a concrete note path via the index and open it; an unresolved
///   wikilink name creates the note and opens it.
pub(crate) fn dispatch_link(app: &mut AppState, target: &LinkTarget, sticky: bool) {
    match target {
        LinkTarget::External(url) => crate::extract::open_external_url(app, url),
        LinkTarget::Zim { archive, article } => open_zim(app, archive, article),
        LinkTarget::VaultPath(_) | LinkTarget::Wikilink(_) => open_note(app, target, sticky),
    }
}

/// Resolve a `zim://<archive>/<article>` target to an open ZIM viewer. The
/// archive authority is mapped to a vault `.zim` path; an authority that
/// matches no scanned archive surfaces a toast rather than a blank viewer.
/// status: widget-mermaid-links
fn open_zim(app: &mut AppState, archive: &str, article: &str) {
    let Ok(root) = app.vault_session.vault.abs_path("") else {
        return;
    };
    match crate::panels::zim::resolve_archive_path(&root, archive) {
        Some(zim_path) => crate::panels::zim::open_at_article(app, &zim_path, article),
        None => app.push_toast(
            format!("No ZIM archive named \u{201c}{archive}\u{201d}"),
            ToastLevel::Warn,
        ),
    }
}

/// Resolve a [`VaultPath`](LinkTarget::VaultPath) / [`Wikilink`](LinkTarget::Wikilink)
/// target to a concrete note path and open it. Uses `core::url::resolve_path`
/// with an existence check + the index-backed name resolver; an unresolved
/// wikilink name creates the note. status: widget-mermaid-links
fn open_note(app: &mut AppState, target: &LinkTarget, sticky: bool) {
    let paths = app
        .vault_session
        .vault
        .walk_indexable_files("")
        .unwrap_or_default();
    let policy = app
        .vault_session
        .config
        .read()
        .map(|c| c.wikilinks.ambiguous_resolution.into())
        .unwrap_or(AmbiguityPolicy::Unresolved);
    let referrer = app
        .session
        .active_tab
        .and_then(|id| app.tab_by_id(id).and_then(|t| t.buffer_path().map(str::to_string)));

    let existing = |p: &str| paths.iter().any(|q| q == p);
    let resolve_name = |name: &str| match wikilink::resolve_path(
        &paths,
        name,
        policy,
        referrer.as_deref(),
    ) {
        Resolution::Resolved(p) => Some(p),
        Resolution::Unresolved | Resolution::Ambiguous(_) => None,
    };

    match url::resolve_path(target, existing, resolve_name) {
        Some(path) => crate::editor_pane::open_file(app, &path, sticky),
        None => {
            // Unresolved: a wikilink name becomes a new note; a path-shaped
            // target that doesn't exist is a dangling file reference — toast it.
            match target {
                LinkTarget::Wikilink(name) => create_and_open(app, name, sticky),
                LinkTarget::VaultPath(p) => app.push_toast(
                    format!("No file at \u{201c}{p}\u{201d}"),
                    ToastLevel::Warn,
                ),
                _ => {}
            }
        }
    }
}

/// Create a new note for an unresolved wikilink name and open it — the same
/// indexer-driven create the wikilink click path uses, so a diagram link to a
/// not-yet-existing note behaves like a wikilink to one. status: widget-mermaid-links
fn create_and_open(app: &mut AppState, name: &str, sticky: bool) {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return;
    }
    let rel = if trimmed.ends_with(".md") {
        trimmed.to_string()
    } else {
        format!("{trimmed}.md")
    };
    let watcher = app.vault_session.services.watcher.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    let rel_owned = rel.clone();
    let result = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(async {
            hiker_core::ops::file::create_at(&watcher, &jobs, &vault, &rel_owned, "").await
        }),
        Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
    };
    match result {
        Ok(actual) => {
            app.file_tree_state.dir_cache.clear();
            app.push_toast(format!("Created {actual}"), ToastLevel::Info);
            crate::editor_pane::open_file(app, &actual, sticky);
        }
        Err(e) => app.push_toast(format!("Couldn't create {rel}: {e}"), ToastLevel::Error),
    }
}

/// Hover tooltips for diagram regions (`widget-diagram-hover-tooltip`). When the
/// pointer is over a diagram-region click zone whose registry entry carries a
/// `tooltip`, paint an egui tooltip at the pointer. Mirrors `wikilink_nav`'s
/// hit-test: translate each widget-local zone rect into screen coords and test
/// the pointer against it.
pub(crate) fn track_hover(
    app: &AppState,
    ctx: &egui::Context,
    editor_rect: egui::Rect,
    click_zones: &[ClickZone],
    registry: &DiagramRegionRegistry,
) {
    let _ = app; // borrow kept for signature symmetry with wikilink_nav.
    let Some(p) = ctx.pointer_latest_pos() else { return };
    if !editor_rect.contains(p) {
        return;
    }
    let lx = p.x - editor_rect.min.x;
    let ly = p.y - editor_rect.min.y;

    let tooltip = click_zones.iter().find_map(|z| {
        let ClickAction::WidgetClick(id) = z.action else {
            return None;
        };
        if id & super::widgets::MERMAID_REGION_TAG == 0 {
            return None;
        }
        if !z.rect.contains(lx, ly) {
            return None;
        }
        registry.get(&id).and_then(|e| e.tooltip.clone())
    });

    if let Some(text) = tooltip {
        egui::show_tooltip_at_pointer(
            ctx,
            egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("diagram-region-tooltip")),
            egui::Id::new("diagram-region-tooltip-content"),
            |ui| ui.label(text),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_routes_each_target() {
        // The classification half of dispatch (the action half needs a live
        // AppState): each shape maps to the LinkTarget dispatch switches on.
        assert!(matches!(
            url::classify("https://example.com"),
            LinkTarget::External(_)
        ));
        assert!(matches!(
            url::classify("zim://wikipedia/C/Rust"),
            LinkTarget::Zim { .. }
        ));
        assert!(matches!(url::classify("folder/Note.md"), LinkTarget::VaultPath(_)));
        assert!(matches!(url::classify("[[Some Note]]"), LinkTarget::Wikilink(_)));
        assert!(matches!(url::classify("Bare Name"), LinkTarget::Wikilink(_)));
    }
}
