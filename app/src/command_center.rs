//! VSCode-style "command center": a centered, clickable search box in
//! the top strip that opens the command palette. Shared between the
//! custom titlebar (when enabled) and a dedicated top bar (otherwise) so
//! it always sits centered in the topmost row regardless of window
//! chrome. Clicking it (or the `Mod-Shift-P` / `Ctrl-K` chords) opens
//! the same palette overlay. [command-center-topbar]

use eframe::egui;

use crate::icons::{self, Icon};
use crate::state::AppState;
use crate::theme;

/// Preferred command-center width: a fraction of the strip, clamped to a
/// sensible pill size.
fn box_width(full_width: f32) -> f32 {
    (full_width * 0.34).clamp(220.0, 480.0)
}

/// Platform-appropriate, ASCII-only shortcut hint (the emoji ban rules
/// out the `⌘`/`⇧` glyphs).
const fn chord_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "Cmd+Shift+P"
    } else {
        "Ctrl+Shift+P"
    }
}

/// The command-center box rect, centered within a top strip's `full`
/// rect. Exposed so the titlebar can carve drag zones around it.
pub fn command_center_rect(full: egui::Rect) -> egui::Rect {
    let w = box_width(full.width());
    let h = (full.height() - 6.0).clamp(20.0, 26.0);
    egui::Rect::from_center_size(egui::pos2(full.center().x, full.center().y), egui::vec2(w, h))
}

impl AppState {
    /// Render the command center centered within `full` (a top strip's
    /// panel rect), overlaid on whatever else the strip drew. Opens the
    /// command palette on click.
    pub fn command_center(&mut self, ui: &mut egui::Ui, full: egui::Rect) {
        let rect = command_center_rect(full);

        let resp = ui
            .interact(rect, ui.id().with("command-center"), egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("Open the command palette");

        let visuals = ui.style().interact(&resp);
        let painter = ui.painter();
        painter.rect(
            rect,
            6.0,
            visuals.bg_fill,
            egui::Stroke::new(1.0, theme::divider()),
            egui::StrokeKind::Inside,
        );

        // Leading search icon.
        let icon_sz = rect.height() * 0.6;
        let icon_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 8.0, rect.center().y - icon_sz / 2.0),
            egui::vec2(icon_sz, icon_sz),
        );
        icons::ICONS
            .image(Icon::Search)
            .tint(theme::muted())
            .paint_at(ui, icon_rect);

        let font = egui::TextStyle::Body.resolve(ui.style());
        painter.text(
            egui::pos2(icon_rect.right() + 6.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            "Search commands",
            font.clone(),
            theme::muted(),
        );
        // Trailing chord hint.
        painter.text(
            egui::pos2(rect.right() - 8.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            chord_hint(),
            egui::FontId::new(font.size * 0.85, font.family.clone()),
            theme::muted().gamma_multiply(0.8),
        );

        if resp.clicked() {
            crate::actions::dispatch(self, "palette.open");
        }
    }

    /// Dedicated command-center top bar — used when the custom titlebar
    /// is off (native window decorations). Renders a thin top panel with
    /// the centered command center.
    pub fn command_center_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("command-center")
            .exact_height(34.0)
            .frame(
                egui::Frame::default()
                    .fill(ctx.style().visuals.panel_fill)
                    .stroke(egui::Stroke::new(1.0, theme::divider())),
            )
            .show(ctx, |ui| {
                let full = ui.max_rect();
                self.command_center(ui, full);
            });
    }
}
