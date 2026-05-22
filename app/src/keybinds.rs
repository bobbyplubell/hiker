//! Window-level keybindings (Mod-W close tab, Ctrl-Tab cycle, Mod-1..9
//! jump). Buffer-local chords (Mod-S save, etc.) stay inside the buffer
//! panel and the editor widget. This module is intentionally tiny — one
//! function called from the top of `update()` after the early-return path
//! for vault switching.
#![allow(clippy::items_after_test_module)]

use eframe::egui;

use crate::editor_pane;
use crate::state::AppState;
use crate::tabs::close_tab_with_dirty_guard;

/// Zero-sized handle for the window-level keybinding catalog. Kept as an
/// inherent method (not a free fn) so the single production caller doesn't
/// trip `single_call_fn`.
pub struct Keybinds;

impl Keybinds {
/// Static list of window-level keybindings surfaced in the help panel.
/// Buffer-local chords (Mod-S etc.) and editor-internal chords (motion,
/// selection) are documented in their respective modules; this list is
/// what gets enumerated in the F1 / `?` help overlay.
pub const fn known_keybindings(self) -> &'static [(&'static str, &'static str)] {
    &[
        ("Mod-S", "Save the active buffer"),
        ("Mod-W", "Close the active tab"),
        ("Mod-,", "Open Settings"),
        ("Ctrl-K", "Open the command palette"),
        ("Ctrl-Space", "Focus the search box"),
        ("Ctrl-Tab", "Cycle to the next tab"),
        ("Shift-Ctrl-Tab", "Cycle to the previous tab"),
        ("Mod-1..9", "Jump to the Nth tab (Mod-9 = last)"),
        ("Alt-Left", "Navigate back through history"),
        ("Alt-Right", "Navigate forward through history"),
        ("Mod-[", "Navigate back through history (macOS)"),
        ("Mod-]", "Navigate forward through history (macOS)"),
        ("Two-finger horizontal swipe", "Back / forward (browser-style)"),
        ("F1 or ?", "Open the help overlay"),
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
        // Every entry must have a non-empty chord and description.
        for (chord, desc) in list {
            assert!(!chord.is_empty(), "empty chord");
            assert!(!desc.is_empty(), "empty description for {chord}");
        }
        // Headline chords the help overlay promises to surface.
        let chords: Vec<&str> = list.iter().map(|(c, _)| *c).collect();
        for required in &["Mod-S", "Mod-W", "Ctrl-Tab", "F1 or ?"] {
            assert!(
                chords.contains(required),
                "known_keybindings missing {required}; got {chords:?}",
            );
        }
    }

    #[test]
    fn known_keybindings_has_no_duplicates() {
        let list = Keybinds.known_keybindings();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (chord, _) in list {
            assert!(seen.insert(chord), "duplicate chord in known_keybindings: {chord}");
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

    // Ctrl-K: open the command palette. The palette is searchable over
    // every registered Action, so it doubles as a discovery surface for
    // commands that don't have their own keybind.
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::K)) {
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

    let _ = shift_cmd;
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
    state.session.active_tab = Some(state.session.tabs[next as usize].id);
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
    state.session.active_tab = Some(state.session.tabs[target].id);
}
}
