//! Window-level keybindings (Mod-W close tab, Ctrl-Tab cycle, Mod-1..9
//! jump). Buffer-local chords (Mod-S save, etc.) stay inside the buffer
//! panel and the editor widget. This module is intentionally tiny — one
//! function called from the top of `update()` after the early-return path
//! for vault switching.
#![allow(clippy::items_after_test_module)]

use eframe::egui;

use crate::editor_pane;
use crate::state::AppState;
use crate::editor_pane::close_tab_with_dirty_guard;

/// Window-level keybinding entry. Carries an action id (`area.verb`), the
/// chord string surfaced by the help overlay, and a human label.
///
/// Action ids drive the command palette ([`crate::panels::command_palette`])
/// and let an override file (deferred) rebind by id rather than chord. The
/// `area` part of the id (everything before the first `.`) is what the
/// palette renders as a small badge.
#[derive(Clone, Copy, Debug)]
pub struct KnownKeybind {
    pub id: &'static str,
    pub chord: &'static str,
    pub label: &'static str,
}

/// Zero-sized handle for the window-level keybinding catalog. Kept as an
/// inherent method (not a free fn) so the single production caller doesn't
/// trip `single_call_fn`.
pub struct Keybinds;

impl Keybinds {
/// Static list of window-level keybindings surfaced in the help panel
/// and the command palette. Buffer-local chords (motion, selection)
/// are documented in their respective modules.
pub const fn known_keybindings(self) -> &'static [KnownKeybind] {
    &[
        KnownKeybind { id: "editor.save",        chord: "Mod-S",          label: "Save the active buffer" },
        KnownKeybind { id: "editor.find",        chord: "Mod-F",          label: "Find in note" },
        KnownKeybind { id: "editor.reader-view", chord: "Mod-Shift-R",    label: "Toggle reader / focus view" },
        KnownKeybind { id: "tab.close",          chord: "Mod-W",          label: "Close the active tab" },
        KnownKeybind { id: "vault.openSettings", chord: "Mod-,",          label: "Open Settings" },
        KnownKeybind { id: "vault.commandPalette", chord: "Mod-Shift-P", label: "Open the command palette" },
        KnownKeybind { id: "vault.commandPaletteCtrlK", chord: "Ctrl-K",  label: "Open the command palette (Ctrl-K)" },
        KnownKeybind { id: "vault.focusSearch",  chord: "Ctrl-Space",     label: "Focus the search box" },
        KnownKeybind { id: "tab.cycleNext",      chord: "Ctrl-Tab",       label: "Cycle to the next tab" },
        KnownKeybind { id: "tab.cyclePrev",      chord: "Shift-Ctrl-Tab", label: "Cycle to the previous tab" },
        KnownKeybind { id: "tab.jumpN",          chord: "Mod-1..9",       label: "Jump to the Nth tab (Mod-9 = last)" },
        KnownKeybind { id: "navigation.back",    chord: "Alt-Left",       label: "Navigate back through history" },
        KnownKeybind { id: "navigation.forward", chord: "Alt-Right",      label: "Navigate forward through history" },
        KnownKeybind { id: "navigation.backMac", chord: "Mod-[",          label: "Navigate back through history (macOS)" },
        KnownKeybind { id: "navigation.forwardMac", chord: "Mod-]",       label: "Navigate forward through history (macOS)" },
        KnownKeybind { id: "navigation.swipe",   chord: "Two-finger horizontal swipe", label: "Back / forward (browser-style)" },
        KnownKeybind { id: "view.toggleHelp",    chord: "F1 or ?",        label: "Open the help overlay" },
    ]
}
}

#[cfg(test)]
mod keybinds_tests {
    use super::*;

    #[test]
    fn known_keybindings_has_required_entries() {
        let list = Keybinds.known_keybindings();
        assert!(list.len() >= 10, "expected at least 10 documented chords, got {}", list.len());
        for k in list {
            assert!(!k.chord.is_empty(), "empty chord");
            assert!(!k.label.is_empty(), "empty description for {}", k.chord);
            assert!(!k.id.is_empty(), "empty id for {}", k.chord);
            assert!(k.id.contains('.'), "action id {} should be area.verb", k.id);
        }
        let chords: Vec<&str> = list.iter().map(|k| k.chord).collect();
        for required in &["Mod-S", "Mod-W", "Ctrl-Tab", "F1 or ?", "Mod-F", "Mod-Shift-R", "Mod-Shift-P"] {
            assert!(
                chords.contains(required),
                "known_keybindings missing {required}; got {chords:?}",
            );
        }
    }

    #[test]
    fn known_keybindings_has_no_duplicates() {
        let list = Keybinds.known_keybindings();
        let mut seen_chords: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut seen_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for k in list {
            assert!(seen_chords.insert(k.chord), "duplicate chord in known_keybindings: {}", k.chord);
            assert!(seen_ids.insert(k.id), "duplicate id in known_keybindings: {}", k.id);
        }
    }
}

impl AppState {
pub fn handle_keybinds(&mut self, ctx: &egui::Context) {
    let state = self;
    // Consume keys so they don't also reach the editor widget. egui's
    // `consume_key` returns true when the chord matched, and prevents the
    // key event from being seen by later handlers in this frame.
    let cmd = egui::Modifiers::COMMAND;
    let shift_cmd = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;

    // Mod-W: close the active tab (with dirty-buffer guard).
    if ctx.input_mut(|i| i.consume_key(cmd, egui::Key::W))
        && let Some(id) = state.session.active_tab
    {
        close_tab_with_dirty_guard(state, id);
    }

    // Ctrl-Tab / Shift-Ctrl-Tab: cycle tabs forward / backward.
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Tab)) {
        cycle_active(state, 1);
    }
    if ctx.input_mut(|i| {
        i.consume_key(
            egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
            egui::Key::Tab,
        )
    }) {
        cycle_active(state, -1);
    }

    // Mod-1..9: jump to the Nth tab (1-indexed). 9 also jumps to the LAST
    // tab regardless of count, matching most editors.
    const NUM_KEYS: [egui::Key; 9] = [
        egui::Key::Num1, egui::Key::Num2, egui::Key::Num3,
        egui::Key::Num4, egui::Key::Num5, egui::Key::Num6,
        egui::Key::Num7, egui::Key::Num8, egui::Key::Num9,
    ];
    for (i, key) in NUM_KEYS.iter().enumerate() {
        if ctx.input_mut(|inp| inp.consume_key(cmd, *key)) {
            state.jump_to_tab(i, /* last_if_n=8 */ i == 8);
        }
    }

    // Alt-Left / Alt-Right: navigate back/forward in history (matches
    // browser semantics; cmd-[ / cmd-] is the macOS alternative).
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowLeft))
        || ctx.input_mut(|i| i.consume_key(cmd, egui::Key::OpenBracket))
    {
        editor_pane::nav_go(state, -1);
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::ArrowRight))
        || ctx.input_mut(|i| i.consume_key(cmd, egui::Key::CloseBracket))
    {
        editor_pane::nav_go(state, 1);
    }

    // Mod-, : open the Settings tab (singleton).
    if ctx.input_mut(|i| i.consume_key(cmd, egui::Key::Comma)) {
        crate::toolbar::open_singleton_tab(state, crate::tab::TabKind::Settings);
    }

    // Mod-F: open the per-buffer find bar (`editor.find`). Routed to
    // the active buffer so we don't open find on a non-buffer tab.
    if ctx.input_mut(|i| i.consume_key(cmd, egui::Key::F)) {
        if let Some(path) = active_buffer_path(state) {
            crate::panels::buffer::find::open(state, &path);
        }
    }

    // Mod-Shift-R: toggle reader / focus view on the active buffer.
    if ctx.input_mut(|i| i.consume_key(shift_cmd, egui::Key::R)) {
        if let Some(path) = active_buffer_path(state) {
            if let Some(b) = state.session.buffers.get_mut(&path) {
                b.reader_view = !b.reader_view;
            }
        }
    }

    // Ctrl-K / Mod-Shift-P: open the command palette. The palette is
    // searchable over every registered keybind, so it doubles as a
    // discovery surface for commands that don't have their own keybind.
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::K))
        || ctx.input_mut(|i| i.consume_key(shift_cmd, egui::Key::P))
    {
        state.ui.palette_open = true;
        state.ui.palette_query.clear();
        state.ui.palette_selected = 0;
    }

    // Ctrl-Space: focus the discovery search box. Independent of Mod
    // mapping so it works on both macOS and Linux/Windows.
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Space)) {
        state.panels.search.focus_query_next_frame = true;
    }

    // F1 or `?`: toggle the help overlay.
    if ctx.input_mut(|i| {
        i.consume_key(egui::Modifiers::NONE, egui::Key::F1)
            || i.consume_key(egui::Modifiers::SHIFT, egui::Key::Slash)
    }) {
        state.ui.show_help = !state.ui.show_help;
    }

    // F12: toggle the puffin profiler overlay. No-op without the
    // `profiling` cargo feature. Also flips puffin's global collection
    // flag so frames stop being recorded when the overlay is hidden.
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F12)) {
        state.ui.show_profiler = !state.ui.show_profiler;
        crate::profiling::set_enabled(state.ui.show_profiler);
    }

    // Shift+F12: dump the currently-captured frames to disk as a
    // `.puffin` binary (round-trippable in the external viewer) plus a
    // `.txt` summary aggregated by scope (readable as a code-review
    // artifact). Lands under `<vault>/.hiker/profiles/`.
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, egui::Key::F12)) {
        state.capture_profile();
    }

    // Two-finger horizontal swipe → back/forward, browser-style. Lives in
    // `widgets::swipe_nav` next to its on-screen indicator.
    state.handle_swipe_nav(ctx);
}
}

/// Dispatch a window-level action by its `known_keybindings` id. The
/// command palette uses this so palette invocations run through the
/// same code path the chord handler runs (per `command-palette`'s
/// "fires the action through the same dispatch path the keybind handler
/// uses"). Returns `false` for unknown ids (no-op) so the caller doesn't
/// need to maintain a parallel allowlist.
pub fn dispatch_action(state: &mut AppState, id: &str) -> bool {
    match id {
        "editor.save" => {
            // Save the active buffer through the editor pane (same path
            // the Mod-S handler buried inside `panels::buffer` uses).
            let Some(path) = active_buffer_path(state) else { return true };
            if let Err(err) = editor_pane::save_buffer(state, &path) {
                state.push_toast(format!("Save failed: {err}"), crate::state::ToastLevel::Error);
            }
        }
        "editor.find" => {
            if let Some(path) = active_buffer_path(state) {
                crate::panels::buffer::find::open(state, &path);
            }
        }
        "editor.reader-view" => {
            if let Some(path) = active_buffer_path(state)
                && let Some(b) = state.session.buffers.get_mut(&path)
            {
                b.reader_view = !b.reader_view;
            }
        }
        "tab.close" => {
            if let Some(id) = state.session.active_tab {
                close_tab_with_dirty_guard(state, id);
            }
        }
        "tab.cycleNext" => cycle_active(state, 1),
        "tab.cyclePrev" => cycle_active(state, -1),
        "vault.openSettings" => {
            crate::toolbar::open_singleton_tab(state, crate::tab::TabKind::Settings);
        }
        "vault.commandPalette" | "vault.commandPaletteCtrlK" => {
            state.ui.palette_open = true;
            state.ui.palette_query.clear();
            state.ui.palette_selected = 0;
        }
        "vault.focusSearch" => {
            state.panels.search.focus_query_next_frame = true;
        }
        "navigation.back" | "navigation.backMac" => editor_pane::nav_go(state, -1),
        "navigation.forward" | "navigation.forwardMac" => editor_pane::nav_go(state, 1),
        "view.toggleHelp" => state.ui.show_help = !state.ui.show_help,
        _ => return false,
    }
    true
}

/// True when the action id is currently dispatchable. Drives the
/// greyed-out rows in the command palette (per `command-palette`).
pub fn is_action_dispatchable(state: &AppState, id: &str) -> bool {
    match id {
        "editor.save" | "editor.find" | "editor.reader-view" => {
            active_buffer_path(state).is_some()
        }
        "tab.close" | "tab.cycleNext" | "tab.cyclePrev" | "tab.jumpN" => {
            state.session.active_tab.is_some()
        }
        _ => true,
    }
}

/// True when the action touches LLM-backed features. Per
/// `command-palette`, those rows hide entirely when `[llm] enabled =
/// false` so the palette doesn't surface dead verbs. None of the v1
/// window-level keybinds currently route into LLM features, but the
/// gate stays in place so future additions just flip this match arm.
pub const fn is_action_ai_touching(_id: &str) -> bool {
    false
}

/// Infer a small "area" badge from the action id's prefix. The palette
/// uses this label to group / colour rows; matches the convention in
/// `command-palette`'s "source area as a small badge" rule.
pub fn action_area_badge(id: &str) -> &'static str {
    match id.split_once('.').map(|(area, _)| area) {
        Some("editor") => "editor",
        Some("tab") => "tab",
        Some("navigation") => "navigation",
        Some("vault") => "vault",
        Some("view") => "view",
        _ => "other",
    }
}

/// Resolve the buffer-map key for the active editor tab, if any. Used by
/// chords that act on the active buffer (find, reader view).
fn active_buffer_path(state: &AppState) -> Option<String> {
    let id = state.session.active_tab?;
    let tab = state.tab_by_id(id)?;
    if let crate::tab::TabKind::Editor { buffer, .. } = &tab.kind {
        Some(crate::buffer::buffer_key_for_source(buffer))
    } else {
        None
    }
}

impl AppState {
fn capture_profile(&mut self) {
    let state = self;
    use crate::state::ToastLevel;
    #[cfg(feature = "profiling")]
    {
        let dir = state.vault_session.vault_root.join(".hiker/profiles");
        if let Err(err) = std::fs::create_dir_all(&dir) {
            state.push_toast(
                format!("Profile dir: {err}"),
                ToastLevel::Error,
            );
            return;
        }
        match crate::profiling::capture_to_file(&dir) {
            Ok((bin, txt)) => state.push_toast(
                format!(
                    "Profile written:\n  {}\n  {}",
                    bin.display(),
                    txt.display()
                ),
                ToastLevel::Info,
            ),
            Err(err) => state.push_toast(
                format!("Profile capture failed: {err}"),
                ToastLevel::Error,
            ),
        }
    }
    #[cfg(not(feature = "profiling"))]
    {
        state.push_toast(
            "Rebuild with `--features profiling` to enable capture",
            ToastLevel::Warn,
        );
    }
}
}

fn cycle_active(state: &mut AppState, delta: i32) {
    if state.session.tabs.is_empty() {
        return;
    }
    let current = state
        .session.active_tab
        .and_then(|id| state.session.tabs.iter().position(|t| t.id == id))
        .unwrap_or(0) as i32;
    let n = state.session.tabs.len() as i32;
    let next = ((current + delta) % n + n) % n;
    crate::state::activate_tab(state, state.session.tabs[next as usize].id);
}

impl AppState {
fn jump_to_tab(&mut self, idx: usize, last_if_n: bool) {
    let state = self;
    if state.session.tabs.is_empty() {
        return;
    }
    let target = if last_if_n && idx + 1 >= state.session.tabs.len() {
        state.session.tabs.len() - 1
    } else if idx < state.session.tabs.len() {
        idx
    } else {
        return;
    };
    crate::state::activate_tab(state, state.session.tabs[target].id);
}
}
