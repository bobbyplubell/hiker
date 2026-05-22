//! Theme overrides for the workbench chrome.
//!
//! Defaults are derived from the ambient `egui::Style`. The host may
//! supply a [`Palette`] from [`crate::Host::theme`]
//! to customise the values that don't map to `egui::Style` directly
//! (accent color, focused-group border, activity bar background, etc.).

use egui::Color32;

/// Theme values used by the workbench chrome. Pull defaults from
/// [`Self::from_egui_style`]; override fields you care about.
#[derive(Clone, Debug)]
pub struct Palette {
    /// Color of the activity bar background strip.
    pub activity_bar_bg: Color32,
    /// Accent color used for the active activity indicator, focused
    /// group border, and other "this is selected" emphasis.
    pub accent: Color32,
    /// Color of the side bar background.
    pub side_bar_bg: Color32,
    /// Color of the focused editor group border.
    pub focused_group_border: Color32,
    /// Width (in points) of the focused editor group border.
    pub focused_group_border_width: f32,
    /// Inset (in points) applied to the content rect inside each tab
    /// pane — gives the content a visible breathing margin around its
    /// edges instead of sitting flush against the tab strip / pane
    /// boundary. Painted in `panel_fill` so it reads as the same grey
    /// as the content background.
    pub pane_content_inset: f32,
    /// Activity bar item width (square).
    pub activity_item_size: f32,
    /// Activity bar total width.
    pub activity_bar_width: f32,
    /// Tab strip height.
    pub tab_bar_height: f32,
}

impl Palette {
    /// Derive a default theme from the ambient egui style.
    ///
    /// The activity bar uses a slightly darker shade than the side bar
    /// (matching the common IDE convention) so the eye can read them as two
    /// distinct panels rather than one continuous dark column. Without
    /// the contrast, an empty side bar visually merges with the activity
    /// bar and looks like wasted space next to the icon strip.
    pub fn from_egui_style(style: &egui::Style) -> Self {
        let visuals = &style.visuals;
        // Derive a slightly darker activity-bar shade from the panel
        // fill (matching the common IDE convention). Without the
        // contrast, an empty side bar visually merges with the
        // activity bar and looks like wasted space next to the icon
        // strip. Lightness shift of -0.05 in normalised channel space,
        // alpha preserved.
        let [r, g, b, a] = visuals.panel_fill.to_array();
        let darken = |v: u8| -> u8 {
            let f = v as f32 / 255.0;
            ((f - 0.05).clamp(0.0, 1.0) * 255.0).round() as u8
        };
        let activity_bar_bg = Color32::from_rgba_unmultiplied(darken(r), darken(g), darken(b), a);
        Self {
            activity_bar_bg,
            accent: visuals.selection.bg_fill,
            side_bar_bg: visuals.panel_fill,
            focused_group_border: visuals.selection.bg_fill,
            focused_group_border_width: 2.0,
            pane_content_inset: 8.0,
            activity_item_size: 28.0,
            activity_bar_width: 48.0,
            tab_bar_height: 28.0,
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::from_egui_style(&egui::Style::default())
    }
}

/// Per-tab style override. Returned from
/// [`crate::Host::tab_style`]. `None` for any field means
/// inherit the ambient style.
#[derive(Clone, Debug, Default)]
pub struct TabStyle {
    /// Override text color of the tab label.
    pub text_color: Option<Color32>,
    /// Override background of the tab's tab-strip entry.
    pub bg_color: Option<Color32>,
    /// Render the title in italic (used internally for Preview tabs).
    pub italic: bool,
}
