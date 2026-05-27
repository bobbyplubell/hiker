//! Per-file pill rendered above the editor widget when a buffer renders a
//! pending-view off the op log. Status: patch-review-file-pill.
//!
//! Drives the bulk verbs (Accept all / Reject all) over the document's
//! pending ops, a Next-hunk navigator that scrolls the editor to the next
//! change by document order, the `(M drifted)` suffix, and — when more than
//! one agent session has pending ops on this document — one selectable row
//! per session (`patch-review-multi-session`). Sibling to
//! `pending_rewrite_banner` (write-shaped proposals); the two strips stack
//! when a path has both kinds of pending op.

use eframe::egui;

use super::diff_overlay::HunkInfo;
use crate::theme;

/// Outcome of a pill-row interaction. Resolved by the buffer panel after
/// the frame is drawn so accept/reject can call into the op log with the
/// buffer panel's `&mut` access to AppState.
#[derive(Clone, Debug, Default)]
pub struct PillAction {
    pub accept_all: bool,
    pub reject_all: bool,
    /// Byte offset to scroll the editor to, if the user clicked Next hunk.
    pub scroll_to_byte: Option<usize>,
    /// Set when the user clicked a session row: the new `active_session`
    /// to scope the diff to (`Some(None)` = all-sessions / unscoped row).
    /// `None` (the outer Option) means no session-row click this frame.
    pub select_session: Option<Option<String>>,
}

/// Drift + per-session breakdown the pill renders alongside the hunk count.
/// Computed by the buffer panel off the op log each frame.
#[derive(Clone, Debug, Default)]
pub struct PillMeta {
    /// Pending ops in the active session that have drifted — feeds the
    /// `(M drifted)` suffix. Accept-all skips them; Reject-all covers them.
    pub drifted: usize,
    /// One entry per distinct `session_id` with pending ops on this doc.
    /// When more than one entry exists the pill renders selectable rows.
    pub sessions: Vec<SessionRow>,
    /// The currently-scoped session (mirrors the buffer's `active_session`)
    /// so the matching row can render as selected.
    pub active_session: Option<String>,
}

/// One agent session with pending ops on the document.
#[derive(Clone, Debug)]
pub struct SessionRow {
    pub session_id: Option<String>,
    pub pending: usize,
}

/// Render context for the per-file patch-review pill. Holds the `ui`
/// so the pill renderer is a single inherent method.
pub struct Pill<'a> {
    pub ui: &'a mut egui::Ui,
}

impl Pill<'_> {
    /// Render the pill if `hunks` has at least one entry; return what the
    /// user asked for (bulk verbs / navigation / session select). No-op
    /// when empty.
    pub fn show(
        &mut self,
        hunks: &[HunkInfo],
        cursor_byte: usize,
        meta: &PillMeta,
    ) -> PillAction {
    let ui = &mut *self.ui;
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
                ui.add(crate::icons::ICONS.image(crate::icons::Icon::Robot));
                // `N hunks (M drifted)` per `patch-review-file-pill`.
                let label = if meta.drifted > 0 {
                    format!(
                        "{} agent hunk{} ({} drifted)",
                        n,
                        if n == 1 { "" } else { "s" },
                        meta.drifted,
                    )
                } else {
                    format!("{} agent hunk{}", n, if n == 1 { "" } else { "s" })
                };
                ui.label(egui::RichText::new(label).small().strong());
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
                            action.scroll_to_byte = HunkNav.next_byte(hunks, cursor_byte);
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
                            .on_hover_text("Accept every pending agent op for this buffer")
                            .clicked()
                        {
                            action.accept_all = true;
                        }
                    },
                );
            });

            // Multi-session rows: only when more than one session has
            // pending ops on the document. Clicking a row scopes the diff
            // to that session (`patch-review-multi-session`).
            if meta.sessions.len() > 1 {
                for row in &meta.sessions {
                    let selected = row.session_id == meta.active_session;
                    let name = row
                        .session_id
                        .as_deref()
                        .unwrap_or("(unscoped)");
                    let text = format!(
                        "Session {}: {} hunk{}",
                        name,
                        row.pending,
                        if row.pending == 1 { "" } else { "s" },
                    );
                    let label = if selected {
                        egui::RichText::new(text).small().strong()
                    } else {
                        egui::RichText::new(text).small().color(theme::muted())
                    };
                    if ui.selectable_label(selected, label).clicked() {
                        action.select_session = Some(row.session_id.clone());
                    }
                }
            }
        });
    action
    }
}

/// Zero-sized next-hunk navigator. A struct (rather than a free fn) so
/// the single prod call site stays an inherent method.
struct HunkNav;

impl HunkNav {
    /// Pick the byte offset to scroll the editor to next: the first change
    /// hunk whose start lies strictly past `cursor_byte`; wraps to the
    /// document-order first hunk at end-of-list.
    fn next_byte(&self, hunks: &[HunkInfo], cursor_byte: usize) -> Option<usize> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_core::diff::HunkKind;

    fn hunk(start: usize, kind: HunkKind) -> HunkInfo {
        HunkInfo {
            byte_start: start,
            byte_end: start + 4,
            op_start: start,
            op_end: start + 4,
            kind,
        }
    }

    #[test]
    fn next_hunk_picks_first_past_cursor() {
        let hunks = vec![
            hunk(10, HunkKind::Modified),
            hunk(50, HunkKind::Added),
            hunk(200, HunkKind::Removed),
        ];
        assert_eq!(HunkNav.next_byte(&hunks, 0), Some(10));
        assert_eq!(HunkNav.next_byte(&hunks, 10), Some(50));
        assert_eq!(HunkNav.next_byte(&hunks, 49), Some(50));
        assert_eq!(HunkNav.next_byte(&hunks, 50), Some(200));
    }

    #[test]
    fn next_hunk_wraps_at_end() {
        let hunks = vec![hunk(10, HunkKind::Modified), hunk(200, HunkKind::Modified)];
        assert_eq!(HunkNav.next_byte(&hunks, 300), Some(10));
    }

    #[test]
    fn next_hunk_ignores_context() {
        let hunks = vec![
            hunk(10, HunkKind::Context),
            hunk(30, HunkKind::Modified),
        ];
        assert_eq!(HunkNav.next_byte(&hunks, 0), Some(30));
    }
}
