//! Wikilink navigation glue for the buffer panel: building the live-title
//! resolver the decoration layer renders pills from, and turning a clicked
//! wikilink pill into an opened note. Kept beside `mod.rs` rather than inside
//! it so the editor render path stays readable and within its length budget;
//! everything here is the app-layer seam between `editor_md::links` (which
//! emits the clickable pills) and `core::wikilink` + the store (which resolve
//! a target to a concrete vault path).

use std::sync::{Arc, Mutex};

use eframe::egui;
use hiker_core::store::Store;
use hiker_core::wikilink::{self, NameResolution};

use crate::state::{AppState, ToastLevel};

/// Build the wikilink live-title resolver: a ULID target maps to its note's
/// current title via the read-store `path → id` table; a name target renders
/// as itself (click-time resolution handles real name lookup). The returned
/// closure owns an `Arc` clone, so it borrows neither `AppState` nor the
/// active buffer. status: wikilink-resolve-store
pub(crate) fn title_resolver(
    read_store: Arc<Mutex<Store>>,
) -> impl Fn(&str) -> Option<String> {
    move |target: &str| {
        if wikilink::looks_like_ulid(target) {
            let store = read_store.lock().ok()?;
            let path = store.path_for_id(target).ok().flatten()?;
            Some(wikilink::title_for_path(&path).to_string())
        } else {
            Some(target.to_string())
        }
    }
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
/// in `text`. A ULID target resolves through the read-store `path → id`
/// table; a name target resolves by unique title match, offering to create
/// the note when nothing matches and surfacing ambiguity / dangling targets
/// as toasts. `sticky` (Mod-click) opens a sticky tab instead of the preview
/// slot. status: wikilink-click-open
fn open_at(app: &mut AppState, text: &str, offset: usize, sticky: bool) {
    let Some(link) = wikilink::parse_links(text)
        .into_iter()
        .find(|l| l.span.start == offset)
    else {
        return;
    };

    if link.is_id_form() {
        let resolved = app
            .vault_session
            .services
            .read_store
            .lock()
            .ok()
            .and_then(|s| s.path_for_id(&link.target).ok().flatten());
        match resolved {
            Some(path) => crate::editor_pane::open_file(app, &path, sticky),
            // Dangling reference: the target ULID's note was deleted.
            None => app.push_toast("Link target missing — the note was deleted", ToastLevel::Warn),
        }
        return;
    }

    let paths = app
        .vault_session
        .vault
        .walk_indexable_files("")
        .unwrap_or_default();
    match wikilink::resolve_name(&paths, &link.target) {
        NameResolution::Unique(path) => crate::editor_pane::open_file(app, &path, sticky),
        NameResolution::Ambiguous => app.push_toast(
            format!("Multiple notes named \u{201c}{}\u{201d} \u{2014} pick one via the [[ menu", link.target),
            ToastLevel::Warn,
        ),
        NameResolution::None => create_and_open(app, &link.target, sticky),
    }
}

/// Create a new note for an unresolved wikilink name and open it. The next
/// save of the linking note normalizes the link to the new note's stamped id
/// (`wikilink-name-normalize`). status: wikilink-unresolved
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
    match app.vault_session.vault.create_note(&rel) {
        Ok(_) => {
            app.session.sidebar.dir_cache.clear();
            app.push_toast(format!("Created {rel}"), ToastLevel::Info);
            crate::editor_pane::open_file(app, &rel, sticky);
        }
        Err(e) => app.push_toast(format!("Couldn't create {rel}: {e}"), ToastLevel::Error),
    }
}
