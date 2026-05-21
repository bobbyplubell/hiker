//! SVG icon set. Each fn returns an `egui::Image` backed by a static
//! byte slice loaded via `include_bytes!`. egui_extras' SVG loader
//! (installed in `HikerApp::new` via `egui_extras::install_image_loaders`)
//! rasterizes the bytes the first time they're requested and caches the
//! result by URI.

use eframe::egui;

macro_rules! icon {
    ($name:ident, $file:literal) => {
        // Some icons in the palette are not currently wired into any panel
        // but are part of the cataloged set; keep them dead-code-allowed.
        #[allow(dead_code)]
        pub fn $name() -> egui::Image<'static> {
            static BYTES: &[u8] = include_bytes!(concat!("../assets/icons/", $file));
            egui::Image::new(egui::ImageSource::Bytes {
                uri: concat!("bytes://icon-", $file).into(),
                bytes: egui::load::Bytes::Static(BYTES),
            })
            .fit_to_exact_size(egui::vec2(14.0, 14.0))
        }
    };
}

icon!(trash, "trash.svg");
icon!(robot, "robot.svg");
icon!(brain, "brain.svg");
icon!(diff, "diff.svg");
icon!(restore, "restore.svg");
icon!(close, "close.svg");
icon!(check, "check.svg");
icon!(cross, "cross.svg");
icon!(search, "search.svg");
icon!(settings, "settings.svg");
icon!(home, "home.svg");
icon!(graph, "graph.svg");
icon!(back, "back.svg");
icon!(forward, "forward.svg");
icon!(folder, "folder.svg");
icon!(expand, "expand.svg");
icon!(collapse, "collapse.svg");
icon!(chevron_up, "chevron_up.svg");
icon!(chevron_down, "chevron_down.svg");
icon!(chevron_left, "chevron_left.svg");
icon!(chevron_right, "chevron_right.svg");
icon!(x, "x.svg");
icon!(eye, "eye.svg");
icon!(wand, "wand.svg");
icon!(bookmark, "bookmark.svg");
icon!(file, "file.svg");
icon!(dot, "dot.svg");
icon!(chat, "chat.svg");
icon!(clock, "clock.svg");
icon!(edit, "edit.svg");
icon!(clipboard, "clipboard.svg");
icon!(plugin, "plugin.svg");
icon!(compass, "compass.svg");
icon!(wrench, "wrench.svg");
icon!(boot, "boot.svg");
// `boot.svg` is the legacy squiggly-trail glyph; `trail()` is the
// preferred name for trail surfaces (sidebar header, editor pill, etc.).
pub fn trail() -> egui::Image<'static> {
    boot()
}
icon!(walk, "walk.svg");
icon!(warning, "warning.svg");
icon!(hourglass, "hourglass.svg");
icon!(blocked, "blocked.svg");
icon!(chart, "chart.svg");
icon!(menu, "menu.svg");
icon!(plus, "plus.svg");
icon!(undo, "undo.svg");
icon!(redo, "redo.svg");
icon!(window_close, "window_close.svg");
icon!(window_maximize, "window_maximize.svg");
icon!(window_minimize, "window_minimize.svg");
icon!(info, "info.svg");
icon!(cluster_tree, "cluster_tree.svg");
icon!(sidebar_left, "sidebar_left.svg");
icon!(sidebar_right, "sidebar_right.svg");

// ---- Semantic helpers --------------------------------------------------
//
// Every place the UI puts a tinted icon should call one of these so the
// "what" (which symbol) and the "how it's coloured" (which role) stay in
// one place. Adding a new role here is cheap; sprinkling raw `.tint(...)`
// calls across panels is what we're trying to avoid.

use crate::theme;

/// Amber-tinted warning glyph: stale buffer, indexer offline, tool error
/// in chat. Pair with `theme::warn()` for matching text colour.
pub fn warn() -> egui::Image<'static> {
    warning().tint(theme::warn())
}

/// White-tinted check used inside coloured "primary affirmative"
/// buttons (Apply staging, accept patch, etc.).
pub fn primary_check() -> egui::Image<'static> {
    check().tint(egui::Color32::WHITE)
}

/// White-tinted cross used inside coloured "primary destructive"
/// buttons (Reject staging, discard, etc.).
pub fn primary_cross() -> egui::Image<'static> {
    cross().tint(egui::Color32::WHITE)
}

/// White-tinted restore used inside coloured "primary restore"
/// buttons (Restore from trash, restore snapshot).
pub fn primary_restore() -> egui::Image<'static> {
    restore().tint(egui::Color32::WHITE)
}

/// White-tinted trash used inside the coloured "permanently delete"
/// button in trash preview.
pub fn primary_trash() -> egui::Image<'static> {
    trash().tint(egui::Color32::WHITE)
}

/// Accent-tinted dot used as a "current selection" marker in the buffer
/// header / status bar (dirty / active indicators).
pub fn current_dot() -> egui::Image<'static> {
    dot().tint(theme::accent())
}
