//! Find-in-note bar pinned to the top of the active buffer panel.
//!
//! Spec: `editor-find-in-note`. The match engine
//! (`editor_view::find::run_search` + `SearchState`) is in the editor crate;
//! this module is the host-side UI bar plus the per-frame debounce / nav /
//! decoration wiring.

use std::time::{Duration, Instant};

use eframe::egui;
use editor_core::selection::Selection;
use editor_view::find::{run_search, search_decorations};

use crate::state::AppState;
use crate::theme;

/// Debounce window for rebuilding the match index off the buffer's text.
/// 150ms matches the typical "debounced as the user types" feel; toggles
/// and Enter / Shift-Enter bypass the debounce by clearing the
/// `query_dirty_at` timestamp through an immediate rebuild.
const REBUILD_DEBOUNCE: Duration = Duration::from_millis(150);
/// How long the `Wrapped to top` / `Wrapped to bottom` footer hint stays
/// visible after a wrap event.
const WRAP_HINT_LIFETIME: Duration = Duration::from_millis(1200);

/// Open the find bar on the given buffer key. Idempotent — re-pressing
/// Mod-F while the bar is open just refocuses the input and re-selects
/// the existing query text. Captures the current selection so Esc can
/// restore it.
pub fn open(app: &mut AppState, buffer_key: &str) {
    let Some(buf) = app.session.buffers.get_mut(buffer_key) else {
        return;
    };
    if !buf.find_ui.open {
        buf.find_ui.saved_selection = Some(buf.editor.selection.clone());
    }
    buf.find_ui.open = true;
    buf.find_ui.focus_next_frame = true;
    buf.view.search.active = true;
    // Force an immediate rebuild on the next paint so opening with a
    // non-empty query repopulates matches without waiting for the
    // debounce.
    buf.find_ui.query_dirty_at = Some(Instant::now() - REBUILD_DEBOUNCE);
}

/// Close the find bar on the given buffer key. Restores the saved
/// selection (if any). Clears the match index so highlight decorations
/// drop on the next rebuild.
pub fn close(app: &mut AppState, buffer_key: &str) {
    let Some(buf) = app.session.buffers.get_mut(buffer_key) else {
        return;
    };
    buf.find_ui.open = false;
    buf.find_ui.regex_error = None;
    buf.find_ui.query_dirty_at = None;
    buf.find_ui.wrapped_hint_at = None;
    buf.view.search.close();
    if let Some(sel) = buf.find_ui.saved_selection.take() {
        buf.editor.selection = sel;
    }
}

/// Compute and push search-match decorations onto `view.decorations` for
/// the active buffer. Called from the per-frame decoration rebuild in
/// `buffer::mod`. Re-running the search per frame here is fine — the
/// match list is small and the call is gated on `view.search.active`.
pub fn push_decorations(
    editor: &editor_core::state::Editor,
    view: &mut editor_view::viewport::ViewState,
) {
    if !view.search.active || view.search.query.is_empty() {
        return;
    }
    let decos = search_decorations(editor, &view.search);
    view.decorations.push(decos);
}

/// Render the find bar inside the buffer panel. Returns the height the
/// bar consumed (caller doesn't actually need it — egui's layout already
/// took the row, but we expose `()` for clarity). Call before the
/// editor / status bar so the bar always pins to the top of the pane.
pub fn render_bar(ui: &mut egui::Ui, app: &mut AppState, buffer_key: &str) {
    // Quick check — bar closed? Nothing to draw.
    let open = app
        .session
        .buffers
        .get(buffer_key)
        .map(|b| b.find_ui.open)
        .unwrap_or(false);
    if !open {
        return;
    }

    // Pull a snapshot of UI inputs we want to mutate. We commit any
    // edits back into the buffer at the end of this function so the
    // borrow of `app.session.buffers` is brief.
    struct Snapshot {
        query: String,
        case_sensitive: bool,
        regex: bool,
        regex_error: Option<String>,
        match_count: usize,
        current_idx: Option<usize>,
        wrapped_hint: Option<(bool, Instant)>,
        focus_next_frame: bool,
    }

    let snap = {
        let buf = match app.session.buffers.get(buffer_key) {
            Some(b) => b,
            None => return,
        };
        Snapshot {
            query: buf.view.search.query.clone(),
            case_sensitive: buf.view.search.flags.case_sensitive,
            regex: buf.view.search.flags.regex,
            regex_error: buf.find_ui.regex_error.clone(),
            match_count: buf.view.search.matches.len(),
            current_idx: buf.view.search.current_idx,
            wrapped_hint: buf
                .find_ui
                .wrapped_hint_at
                .map(|t| (buf.find_ui.wrapped_forward, t)),
            focus_next_frame: buf.find_ui.focus_next_frame,
        }
    };

    enum Verb {
        Next,
        Prev,
        Close,
    }

    let mut new_query = snap.query.clone();
    let mut new_case = snap.case_sensitive;
    let mut new_regex = snap.regex;
    let mut verb: Option<Verb> = None;
    let mut input_resp: Option<egui::Response> = None;

    let bar_border = if snap.regex && snap.regex_error.is_some() {
        egui::Stroke::new(1.0, egui::Color32::from_rgb(0xc0, 0x39, 0x2b))
    } else {
        egui::Stroke::new(1.0, egui::Color32::from_gray(200))
    };

    egui::Frame::default()
        .fill(ui.visuals().extreme_bg_color)
        .stroke(bar_border)
        .inner_margin(egui::Margin::symmetric(6, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Find").small().color(theme::muted()));
                let edit = egui::TextEdit::singleline(&mut new_query)
                    .hint_text("Find in note")
                    .desired_width(220.0);
                let resp = ui.add(edit);
                // Forward Enter / Shift-Enter to navigation. Doing the
                // capture here (instead of as a window-level chord)
                // keeps the chord scoped to the find input and lets the
                // editor see Enter when the bar is closed.
                if resp.has_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                {
                    if ui.input(|i| i.modifiers.shift) {
                        verb = Some(Verb::Prev);
                    } else {
                        verb = Some(Verb::Next);
                    }
                }
                if resp.has_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Escape))
                {
                    verb = Some(Verb::Close);
                }
                input_resp = Some(resp);

                // Count badge: "N of M" / "No matches".
                let badge = if new_query.is_empty() {
                    String::new()
                } else if snap.regex && snap.regex_error.is_some() {
                    "Invalid regex".to_string()
                } else if snap.match_count == 0 {
                    "No matches".to_string()
                } else {
                    let n = snap.current_idx.map(|i| i + 1).unwrap_or(1);
                    format!("{} of {}", n, snap.match_count)
                };
                if !badge.is_empty() {
                    ui.label(
                        egui::RichText::new(badge)
                            .small()
                            .color(theme::muted()),
                    );
                }

                if ui
                    .small_button("\u{25B2}")
                    .on_hover_text("Previous match (Shift-Enter)")
                    .clicked()
                {
                    verb = Some(Verb::Prev);
                }
                if ui
                    .small_button("\u{25BC}")
                    .on_hover_text("Next match (Enter)")
                    .clicked()
                {
                    verb = Some(Verb::Next);
                }
                ui.toggle_value(&mut new_case, "Aa")
                    .on_hover_text("Match case");
                ui.toggle_value(&mut new_regex, ".*")
                    .on_hover_text("Regular expression");
                if ui
                    .small_button("\u{00D7}")
                    .on_hover_text("Close (Esc)")
                    .clicked()
                {
                    verb = Some(Verb::Close);
                }
            });

            // Error line under the bar when the regex is invalid.
            if snap.regex && let Some(err) = &snap.regex_error {
                ui.label(
                    egui::RichText::new(err)
                        .small()
                        .color(egui::Color32::from_rgb(0xc0, 0x39, 0x2b)),
                );
            }

            // One-shot wrapped hint in the footer.
            if let Some((forward, t)) = snap.wrapped_hint
                && t.elapsed() < WRAP_HINT_LIFETIME
            {
                let text = if forward {
                    "Wrapped to top"
                } else {
                    "Wrapped to bottom"
                };
                ui.label(
                    egui::RichText::new(text)
                        .small()
                        .italics()
                        .color(theme::muted()),
                );
            }
        });

    if snap.focus_next_frame
        && let Some(resp) = input_resp
    {
        resp.request_focus();
    }

    // Commit query / toggle edits.
    let query_changed = new_query != snap.query;
    let toggles_changed = new_case != snap.case_sensitive || new_regex != snap.regex;
    if let Some(buf) = app.session.buffers.get_mut(buffer_key) {
        buf.view.search.query = new_query;
        buf.view.search.flags.case_sensitive = new_case;
        buf.view.search.flags.regex = new_regex;
        if query_changed {
            buf.find_ui.query_dirty_at = Some(Instant::now());
        }
        if toggles_changed {
            // Toggles bypass debounce — they apply immediately.
            buf.find_ui.query_dirty_at = Some(Instant::now() - REBUILD_DEBOUNCE);
        }
        if snap.focus_next_frame {
            buf.find_ui.focus_next_frame = false;
        }
        // Stale wrapped hint — clear after its lifetime so it doesn't
        // hang around silently in the panel cache.
        if let Some(t) = buf.find_ui.wrapped_hint_at
            && t.elapsed() >= WRAP_HINT_LIFETIME
        {
            buf.find_ui.wrapped_hint_at = None;
        }
    }

    // Apply navigation / close verbs.
    match verb {
        Some(Verb::Next) => step(app, buffer_key, 1),
        Some(Verb::Prev) => step(app, buffer_key, -1),
        Some(Verb::Close) => close(app, buffer_key),
        None => {}
    }
}

/// Per-frame match-index rebuild: debounced on query edits, immediate on
/// toggle flips. Called after `render_bar` so this frame's edits are
/// reflected. Keeps the host UI as the single point of entry into
/// `run_search` — the editor crate doesn't drive it on its own.
pub fn tick_rebuild(app: &mut AppState, buffer_key: &str) {
    let Some(buf) = app.session.buffers.get_mut(buffer_key) else {
        return;
    };
    if !buf.view.search.active {
        return;
    }
    let Some(dirty_at) = buf.find_ui.query_dirty_at else {
        return;
    };
    if dirty_at.elapsed() < REBUILD_DEBOUNCE {
        return;
    }
    buf.find_ui.query_dirty_at = None;

    // Regex validation: surface a one-line error and clear matches so
    // highlights drop until the pattern parses. Substring search has no
    // failure mode beyond "empty query" → no matches.
    if buf.view.search.flags.regex && !buf.view.search.query.is_empty() {
        let pat = if buf.view.search.flags.case_sensitive {
            buf.view.search.query.clone()
        } else {
            format!("(?i){}", buf.view.search.query)
        };
        if let Err(err) = regex::Regex::new(&pat) {
            buf.find_ui.regex_error = Some(format!("Invalid regex: {err}"));
            buf.view.search.matches.clear();
            buf.view.search.current_idx = None;
            return;
        }
    }
    buf.find_ui.regex_error = None;

    let matches = run_search(&buf.editor, &buf.view.search.query, buf.view.search.flags);
    buf.view.search.matches = matches;
    // Choose an initial current_idx based on the cursor's position so
    // hitting Mod-F → Enter jumps to the nearest forward match.
    if buf.view.search.current_idx.is_none() && !buf.view.search.matches.is_empty() {
        let cursor = buf.editor.selection.main().head.offset();
        let idx = buf
            .view
            .search
            .matches
            .iter()
            .position(|r| r.start >= cursor)
            .unwrap_or(0);
        buf.view.search.current_idx = Some(idx);
        focus_match(buf);
    } else if let Some(idx) = buf.view.search.current_idx {
        // Clamp the index if the match count shrank.
        if idx >= buf.view.search.matches.len() {
            buf.view.search.current_idx = if buf.view.search.matches.is_empty() {
                None
            } else {
                Some(buf.view.search.matches.len() - 1)
            };
            if buf.view.search.current_idx.is_some() {
                focus_match(buf);
            }
        }
    }
}

/// Advance / retreat the active match by `delta` (±1), wrapping at the
/// ends with a one-shot footer hint. Moves the cursor to the new match.
fn step(app: &mut AppState, buffer_key: &str, delta: i32) {
    let Some(buf) = app.session.buffers.get_mut(buffer_key) else {
        return;
    };
    // Force a fresh rebuild before stepping so navigating after a
    // freshly-typed query (or a regex flip) reflects the current text.
    let _ = buf;
    tick_rebuild(app, buffer_key);
    let Some(buf) = app.session.buffers.get_mut(buffer_key) else {
        return;
    };
    if buf.view.search.matches.is_empty() {
        return;
    }
    let n = buf.view.search.matches.len() as i32;
    let cur = buf.view.search.current_idx.map(|i| i as i32).unwrap_or(-1);
    let raw = cur + delta;
    let wrapped = raw < 0 || raw >= n;
    let next = ((raw % n) + n) % n;
    buf.view.search.current_idx = Some(next as usize);
    if wrapped {
        buf.find_ui.wrapped_hint_at = Some(Instant::now());
        buf.find_ui.wrapped_forward = delta > 0;
    }
    focus_match(buf);
}

/// Move the cursor + scroll to the currently-active match.
fn focus_match(buf: &mut crate::buffer::Buffer) {
    let Some(idx) = buf.view.search.current_idx else {
        return;
    };
    let Some(m) = buf.view.search.matches.get(idx).cloned() else {
        return;
    };
    buf.editor.selection = Selection::single(m.start);
    buf.view.scroll_caret_into_view = true;
}
