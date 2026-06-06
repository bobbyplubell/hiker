//! Wikilink navigation glue for the buffer panel: building the live-title
//! resolver the decoration layer renders pills from, and turning a clicked
//! wikilink pill into an opened note. Kept beside `mod.rs` rather than inside
//! it so the editor render path stays readable and within its length budget;
//! everything here is the app-layer seam between `editor_md::links` (which
//! emits the clickable pills) and `core::wikilink` + the store (which resolve
//! a target to a concrete vault path).

use std::sync::{Arc, Mutex};

use eframe::egui;
use editor_view::viewport::{ClickAction, ClickZone};
use hiker_core::store::Store;
use hiker_core::wikilink::{self, AmbiguityPolicy, Resolution};

use crate::state::{AppState, ToastLevel};

/// Build the wikilink live-title resolver. Under the path-form
/// (`wikilink-path-form`) the target *is* the path-or-name the user
/// typed; the resolver hands back a display label by stripping `.md`
/// from the basename. A click-time resolver (below) does the actual
/// path lookup. The returned closure owns an `Arc` clone, so it
/// borrows neither `AppState` nor the active buffer.
///
/// The `read_store` borrow is kept on the signature for forward
/// compatibility with the frontmatter-title path (`wikilink-render`
/// resolves the target's current `title` frontmatter when set);
/// today the resolver doesn't read it yet.
///
/// status: wikilink-render
pub(crate) fn title_resolver(
    _read_store: Arc<Mutex<Store>>,
) -> impl Fn(&str) -> Option<String> {
    move |target: &str| Some(wikilink::title_for_path(target).to_string())
}

/// Dispatch this frame's wikilink pill clicks. Each tagged id carries the
/// link's full-span start byte; re-parse the link there against the active
/// buffer's current text and open the target. status: wikilink-click-open
pub(crate) fn handle_clicks(
    app: &mut AppState,
    ctx: &egui::Context,
    path: &str,
    clicks: &[u64],
    mod_click: bool,
) {
    if clicks.is_empty() {
        return;
    }
    let Some(text) = app
        .session
        .buffers
        .get(path)
        .map(crate::buffer::Buffer::current_text)
    else {
        return;
    };
    for &id in clicks {
        let offset = (id & !editor_md::links::WIKILINK_WIDGET_TAG) as usize;
        open_at(app, &text, offset, mod_click);
    }
    ctx.request_repaint();
}

/// Resolve and open the wikilink whose full `[[…]]` span starts at `offset`
/// in `text`. Path-form (`wikilink-resolve`): a bare-name target matches
/// by basename; an explicit-path target matches by exact path. Ambiguity
/// policy is read from `[wikilinks] ambiguous_resolution`.
/// `sticky` (Mod-click) opens a sticky tab instead of the preview slot.
///
/// status: wikilink-click-open
fn open_at(app: &mut AppState, text: &str, offset: usize, sticky: bool) {
    let Some(link) = wikilink::parse_links(text)
        .into_iter()
        .find(|l| l.span.start == offset)
    else {
        return;
    };

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
    match wikilink::resolve_path(&paths, &link.target, policy, referrer.as_deref()) {
        Resolution::Resolved(path) => crate::editor_pane::open_file(app, &path, sticky),
        Resolution::Ambiguous(_) => app.push_toast(
            format!(
                "Multiple notes named \u{201c}{}\u{201d} \u{2014} pick one via the [[ menu",
                link.target
            ),
            ToastLevel::Warn,
        ),
        Resolution::Unresolved => create_and_open(app, &link.target, sticky),
    }
}

/// Create a new note for an unresolved wikilink name and open it. Under
/// path-based identity (`wikilink-path-form`) the link the user typed
/// (a name) resolves to the new note's path on the next decoration rebuild;
/// no save-time rewrite. status: wikilink-unresolved
///
/// Routes through the indexer-driven `core::ops::file::create_at` (watcher
/// suppression + `IndexJob::Upsert`) rather than the bare `vault::create_note`
/// so the new note is indexed without a duplicate watcher-driven ingest — the
/// same discipline as the `+` new-item button.
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

// ---------------------------------------------------------------------------
// Hover preview — [wikilink-hover-preview]
// ---------------------------------------------------------------------------

/// Per-frame entry point from the buffer panel. If the pointer is over a
/// wikilink pill that resolves to a concrete note, register a hover on the
/// shared note-preview mechanism (`preview-note-hover`) — the same one the
/// file-tree / canvas sidebars use, which renders a live markdown preview
/// (with diagrams) of the target. The actual draw happens at the frame-loop
/// level in `widgets::preview::render_note_preview`, gated by the
/// `[ui].hover_previews_enabled` toggle, so there's no per-buffer lifecycle
/// state to keep here.
///
/// `editor_rect` is the screen-space rect of the editor body — click zones
/// are widget-local, so we translate them into screen coords here. `ui` is
/// borrowed read-only (the registration only reads `ui.ctx()` time/pointer).
pub(crate) fn track_hover(
    app: &AppState,
    ui: &egui::Ui,
    path: &str,
    editor_rect: egui::Rect,
    click_zones: &[ClickZone],
) {
    let pointer = ui.ctx().pointer_latest_pos();

    // Which wikilink pill (if any) is the pointer over? Translate the
    // widget-local zone rect to screen coords so the preview anchors on it.
    let pill_under_pointer: Option<(usize, egui::Rect)> = pointer.and_then(|p| {
        if !editor_rect.contains(p) {
            return None;
        }
        let lx = p.x - editor_rect.min.x;
        let ly = p.y - editor_rect.min.y;
        click_zones.iter().find_map(|z| {
            let ClickAction::WidgetClick(id) = z.action else {
                return None;
            };
            if id & editor_md::links::WIKILINK_WIDGET_TAG == 0 {
                return None;
            }
            if !z.rect.contains(lx, ly) {
                return None;
            }
            let offset = (id & !editor_md::links::WIKILINK_WIDGET_TAG) as usize;
            let screen = egui::Rect::from_min_max(
                egui::pos2(editor_rect.min.x + z.rect.x_min, editor_rect.min.y + z.rect.y_min),
                egui::pos2(editor_rect.min.x + z.rect.x_max, editor_rect.min.y + z.rect.y_max),
            );
            Some((offset, screen))
        })
    });

    // Only resolved links preview; unresolved / ambiguous show nothing.
    if let Some((offset, anchor)) = pill_under_pointer
        && let Some(target) = resolve_target_path(app, path, offset)
    {
        // Interactive registration: the wikilink preview anchors at the pill,
        // scrolls under the wheel, and survives the cursor sliding onto it
        // (unlike the passive, side-anchored sidebar previews).
        crate::widgets::preview::register_note_hover_interactive(ui, anchor, &target);
    }
}

/// Resolve the wikilink whose full `[[…]]` span starts at `offset` in the
/// active buffer to its concrete vault path, or `None` for unresolved /
/// ambiguous links (no preview).
fn resolve_target_path(app: &AppState, path: &str, offset: usize) -> Option<String> {
    let text = app.session.buffers.get(path).map(crate::buffer::Buffer::current_text)?;
    let link = wikilink::parse_links(&text)
        .into_iter()
        .find(|l| l.span.start == offset)?;
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
    match wikilink::resolve_path(&paths, &link.target, policy, Some(path)) {
        Resolution::Resolved(p) => Some(p),
        Resolution::Unresolved | Resolution::Ambiguous(_) => None,
    }
}
