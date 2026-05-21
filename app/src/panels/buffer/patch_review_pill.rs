//! Per-file pill rendered above the editor widget when a buffer has
//! hydrated pending agent edits. Status: patch-review-file-pill.
//!
//! Drives the bulk verbs (Accept all / Reject all) over the buffer's
//! `hydrated_proposals` list plus a Next-hunk navigator that scrolls the
//! editor to the next change by document order. Sibling to
//! `pending_rewrite_banner` (write-shaped proposals); the two strips stack
//! when a path has both kinds of pending proposals.

use eframe::egui;

use super::diff_overlay::HunkInfo;
use crate::state::AppState;
use crate::theme;

/// Outcome of a pill-row interaction. Resolved by the buffer panel after
/// the frame is drawn so accept/reject can call into the staging service
/// with the buffer panel's `&mut` access to AppState.
#[derive(Clone, Debug, Default)]
pub struct PillAction {
    pub accept_all: bool,
    pub reject_all: bool,
    /// Byte offset to scroll the editor to, if the user clicked Next hunk.
    pub scroll_to_byte: Option<usize>,
}

/// Render the pill if `hunks` has at least one entry; return what the user
/// asked for (bulk verbs / navigation). No-op when the slice is empty.
pub fn show(
    ui: &mut egui::Ui,
    app: &AppState,
    hunks: &[HunkInfo],
    cursor_byte: usize,
) -> PillAction {
    let mut action = PillAction::default();
    if hunks.is_empty() {
        return action;
    }

    let n = hunks.iter().filter(|h| h.is_change()).count();
    let bg = egui::Color32::from_rgb(0xe6, 0xf3, 0xe9);
    let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(0xa0, 0xc7, 0xa8));
    egui::Frame::default()
        .fill(bg)
        .stroke(stroke)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(crate::icons::robot());
                let label = format!("{} agent hunk{}", n, if n == 1 { "" } else { "s" });
                ui.label(egui::RichText::new(label).small().strong());
                let _ = app;
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let next_enabled = n > 0;
                        if ui
                            .add_enabled(next_enabled, egui::Button::new(
                                egui::RichText::new("Next hunk").small(),
                            ))
                            .on_hover_text("Scroll to the next hunk")
                            .clicked()
                        {
                            action.scroll_to_byte = next_hunk_byte(hunks, cursor_byte);
                        }
                        if ui
                            .add_enabled(
                                n > 0,
                                egui::Button::new(
                                    egui::RichText::new("Reject all").color(egui::Color32::WHITE),
                                )
                                .fill(egui::Color32::from_rgb(0xb9, 0x3a, 0x3a)),
                            )
                            .clicked()
                        {
                            action.reject_all = true;
                        }
                        if ui
                            .add_enabled(
                                n > 0,
                                egui::Button::new(
                                    egui::RichText::new("Accept all").color(egui::Color32::WHITE),
                                )
                                .fill(egui::Color32::from_rgb(0x2f, 0x8f, 0x4d)),
                            )
                            .on_hover_text("Accept every hydrated proposal for this buffer")
                            .clicked()
                        {
                            action.accept_all = true;
                        }
                        let _ = theme::muted;
                    },
                );
            });
        });
    action
}

/// Pick the byte offset to scroll the editor to next: the first change
/// hunk whose start lies strictly past `cursor_byte`; wraps to the
/// document-order first hunk at end-of-list.
fn next_hunk_byte(hunks: &[HunkInfo], cursor_byte: usize) -> Option<usize> {
    let mut starts: Vec<usize> = hunks
        .iter()
        .filter(|h| h.is_change())
        .map(|h| h.byte_start)
        .collect();
    if starts.is_empty() {
        return None;
    }
    starts.sort_unstable();
    starts
        .iter()
        .find(|&&b| b > cursor_byte)
        .copied()
        .or_else(|| starts.first().copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_core::diff::HunkKind;

    fn hunk(start: usize, kind: HunkKind) -> HunkInfo {
        HunkInfo { byte_start: start, byte_end: start + 4, kind }
    }

    #[test]
    fn next_hunk_picks_first_past_cursor() {
        let hunks = vec![
            hunk(10, HunkKind::Modified),
            hunk(50, HunkKind::Added),
            hunk(200, HunkKind::Removed),
        ];
        assert_eq!(next_hunk_byte(&hunks, 0), Some(10));
        assert_eq!(next_hunk_byte(&hunks, 10), Some(50));
        assert_eq!(next_hunk_byte(&hunks, 49), Some(50));
        assert_eq!(next_hunk_byte(&hunks, 50), Some(200));
    }

    #[test]
    fn next_hunk_wraps_at_end() {
        let hunks = vec![hunk(10, HunkKind::Modified), hunk(200, HunkKind::Modified)];
        assert_eq!(next_hunk_byte(&hunks, 300), Some(10));
    }

    #[test]
    fn next_hunk_ignores_context() {
        let hunks = vec![
            hunk(10, HunkKind::Context),
            hunk(30, HunkKind::Modified),
        ];
        assert_eq!(next_hunk_byte(&hunks, 0), Some(30));
    }
}
