//! In-buffer find/replace overlay. Operates on the active text buffer
//! directly, building `Transaction`s for replacements.
//!
//! Naive O(N) substring scan — fine for Phase 1; the editor crate may
//! grow a richer search primitive later.

use editor_core::change::Set;
use editor_core::transaction::Transaction;

use crate::host::Buffer;

#[derive(Default)]
pub struct FindReplace {
    pub open: bool,
    pub show_replace: bool,
    pub query: String,
    pub replacement: String,
}

impl FindReplace {
    pub const fn open_find(&mut self) {
        self.open = true;
        self.show_replace = false;
    }

    pub const fn open_replace(&mut self) {
        self.open = true;
        self.show_replace = true;
    }

    pub fn ui(&mut self, ctx: &egui::Context, buffer: Option<&mut Buffer>) {
        if !self.open {
            return;
        }
        let mut close = false;
        let mut action: Option<Action> = None;
        egui::Window::new("find")
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 12.0))
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Find:");
                    ui.text_edit_singleline(&mut self.query);
                    if ui.button("Next").clicked() {
                        action = Some(Action::FindNext);
                    }
                    if ui.button("x").clicked() {
                        close = true;
                    }
                });
                if self.show_replace {
                    ui.horizontal(|ui| {
                        ui.label("Replace:");
                        ui.text_edit_singleline(&mut self.replacement);
                        if ui.button("Replace next").clicked() {
                            action = Some(Action::ReplaceNext);
                        }
                        if ui.button("Replace all").clicked() {
                            action = Some(Action::ReplaceAll);
                        }
                    });
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
        if let Some(buffer) = buffer
            && let Some(act) = action
        {
            apply(act, buffer, &self.query, &self.replacement);
        }
        if close {
            self.open = false;
        }
    }
}

#[derive(Clone, Copy)]
enum Action {
    FindNext,
    ReplaceNext,
    ReplaceAll,
}

fn apply(action: Action, buffer: &mut Buffer, query: &str, replacement: &str) {
    if query.is_empty() {
        return;
    }
    let text = buffer.contents();
    let caret = buffer.state.selection.main().head.offset();
    match action {
        Action::FindNext => {
            if let Some(pos) = find_next(&text, caret, query) {
                use editor_core::selection::{SelRange, Selection};
                let range = SelRange::new(pos, pos + query.len());
                buffer.state.selection = Selection::from_ranges(vec![range], 0);
            }
        }
        Action::ReplaceNext => {
            if let Some(pos) = find_next(&text, caret, query) {
                let tx = build_replace_tx(buffer, &[(pos, query.len())], replacement);
                buffer.state = buffer.state.apply(tx);
            }
        }
        Action::ReplaceAll => {
            let positions: Vec<(usize, usize)> = find_all(&text, query)
                .into_iter()
                .map(|p| (p, query.len()))
                .collect();
            if positions.is_empty() {
                return;
            }
            let tx = build_replace_tx(buffer, &positions, replacement);
            buffer.state = buffer.state.apply(tx);
        }
    }
}

fn find_next(haystack: &str, from: usize, needle: &str) -> Option<usize> {
    let from = from.min(haystack.len());
    haystack[from..]
        .find(needle)
        .map(|p| p + from)
        .or_else(|| haystack.find(needle))
}

fn find_all(haystack: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(p) = haystack[from..].find(needle) {
        let abs = from + p;
        out.push(abs);
        from = abs + needle.len().max(1);
    }
    out
}

fn build_replace_tx(buffer: &Buffer, ranges: &[(usize, usize)], replacement: &str) -> Transaction {
    let doc_len = buffer.state.doc.len_bytes();
    let edits = ranges
        .iter()
        .map(|(pos, len)| ((*pos..pos + len), replacement.to_string()));
    Transaction::new(Set::of(doc_len, edits))
}
