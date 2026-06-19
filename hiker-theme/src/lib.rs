//! Hiker's egui theme. Approximates the CSS tokens from
//! `ui/src/style/tokens.css` — light palette to match the editor's
//! `light_default` theme used for markdown decorations.
//!
//! Shared egui theme/style (status: style-theme-install), extracted from
//! `app/src/theme.rs`. Depends ONLY on egui; consumed by both `hiker-app` and
//! `hiker-crawler` so the two match colors/fonts/spacing.

/// Shared corner radius for buttons and button-like controls (status:
/// style-button-radius). `egui::Button` reads it from `widgets.*.corner_radius`;
/// `egui::ImageButton` (whose rounding comes from the image, not the widget
/// visuals) must opt in with `.corner_radius(BUTTON_CORNER_RADIUS)`.
pub const BUTTON_CORNER_RADIUS: u8 = 5;

/// Zero-sized handle for installing the app theme. A struct (rather than
/// a free `install` fn) so it's an inherent method, exempt from
/// `single_call_fn`.
pub struct Theme;

impl Theme {
    pub fn install(self, ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();

        style.visuals = egui::Visuals::light();
        style.visuals.window_fill = egui::Color32::from_rgb(0xfa, 0xfb, 0xfc);
        style.visuals.panel_fill = egui::Color32::from_rgb(0xf4, 0xf6, 0xf8);
        style.visuals.faint_bg_color = egui::Color32::from_rgb(0xec, 0xef, 0xf3);
        style.visuals.extreme_bg_color = egui::Color32::from_rgb(0xff, 0xff, 0xff);
        style.visuals.code_bg_color = egui::Color32::from_rgb(0xee, 0xf1, 0xf5);

        style.visuals.widgets.noninteractive.bg_stroke =
            egui::Stroke::new(1.0, egui::Color32::from_rgb(0xd6, 0xda, 0xe0));
        style.visuals.widgets.noninteractive.fg_stroke =
            egui::Stroke::new(1.0, egui::Color32::from_rgb(0x1f, 0x24, 0x2c));
        style.visuals.widgets.inactive.fg_stroke =
            egui::Stroke::new(1.0, egui::Color32::from_rgb(0x4a, 0x52, 0x5e));

        // Ghost buttons (status: style-ghost-button): no background or border at
        // rest, a subtle fill + 1px border on hover, a deeper fill + accent
        // border when pressed. Applied through the interact widget visuals so
        // every button — text, icon, and the split-add control — inherits it.
        // `corner_radius` (status: style-button-radius) rounds the hover/press
        // fills to the shared token.
        let radius = egui::CornerRadius::same(BUTTON_CORNER_RADIUS);
        let w = &mut style.visuals.widgets;
        w.inactive.corner_radius = radius;
        w.hovered.corner_radius = radius;
        w.active.corner_radius = radius;
        w.open.corner_radius = radius;
        w.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
        w.inactive.bg_stroke = egui::Stroke::NONE;
        w.hovered.weak_bg_fill = hover_bg();
        w.hovered.bg_stroke = egui::Stroke::new(1.0, divider());
        w.active.weak_bg_fill = active_bg();
        w.active.bg_stroke = egui::Stroke::new(1.0, accent());

        let accent = egui::Color32::from_rgb(0x2f, 0x6f, 0xed);
        // Selection highlight: accent blue at ~44 % opacity (unmultiplied).
        // The old value used pre-multiplied RGBA with very diluted channels,
        // producing a near-invisible tint on the light panel background (#f4f6f8).
        // This raises contrast so selections are clearly readable across all
        // text inputs and the editor without being visually heavy.
        style.visuals.selection.bg_fill =
            egui::Color32::from_rgba_unmultiplied(0x2f, 0x6f, 0xed, 0x70);
        style.visuals.selection.stroke = egui::Stroke::new(1.0, accent);
        style.visuals.hyperlink_color = accent;

        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);

        // Body text a touch larger than egui's default — markdown editing
        // wants more readability than UI controls.
        let mut text_styles = style.text_styles.clone();
        if let Some(body) = text_styles.get_mut(&egui::TextStyle::Body) {
            body.size = 14.0;
        }
        if let Some(monospace) = text_styles.get_mut(&egui::TextStyle::Monospace) {
            monospace.size = 13.0;
        }
        style.text_styles = text_styles;

        ctx.set_style(style);
    }
}

/// Subtle border / divider colour used by panels and the tab strip.
pub const fn divider() -> egui::Color32 {
    egui::Color32::from_rgb(0xd6, 0xda, 0xe0)
}

/// Slightly-darker highlight for the active tab / selected row.
pub const fn active_bg() -> egui::Color32 {
    egui::Color32::from_rgb(0xe2, 0xe8, 0xf0)
}

/// Hover background tint.
pub const fn hover_bg() -> egui::Color32 {
    egui::Color32::from_rgb(0xea, 0xee, 0xf4)
}

/// The canonical "click acts here" hover signal (`docs/interaction.md`
/// [hover-open-signal]): an openable row/card paints THIS wash — [`active_bg`]
/// while the row is the active/selected item, [`hover_bg`] on hover, nothing
/// otherwise — alongside `CursorIcon::PointingHand`. One signal per meaning:
/// surfaces that paint their row/card background by hand route the colour
/// decision through here instead of hand-rolling a wash, and widget-based rows
/// inherit the same [`hover_bg`] from the installed style's
/// `widgets.hovered.weak_bg_fill`.
pub const fn open_signal_wash(active: bool, hovered: bool) -> Option<egui::Color32> {
    if active {
        Some(active_bg())
    } else if hovered {
        Some(hover_bg())
    } else {
        None
    }
}

/// Accent colour for dirty markers, focus rings, etc.
pub const fn accent() -> egui::Color32 {
    egui::Color32::from_rgb(0x2f, 0x6f, 0xed)
}

/// Muted text colour for secondary labels (vault path, status bar).
pub const fn muted() -> egui::Color32 {
    egui::Color32::from_rgb(0x6a, 0x73, 0x7d)
}

/// Amber used for in-line warning glyphs and matching warning text
/// (stale-buffer hint, index-offline hint, tool-error chat badges).
pub const fn warn() -> egui::Color32 {
    egui::Color32::from_rgb(0xc4, 0x86, 0x00)
}

/// Vault-graph kind palette (`vault-graph-kind-nodes`): one hue per container
/// kind, shared by the node fill, its membership edges, and the toolbar
/// filter labels — the toolbar doubles as the legend. Plain notes keep the
/// engine's user-editable flat node color, so only containers live here.
pub const fn kind_board() -> egui::Color32 {
    egui::Color32::from_rgb(0xc9, 0x7b, 0x2a)
}

/// Trail-doc hue (see [`kind_board`]).
pub const fn kind_trail() -> egui::Color32 {
    egui::Color32::from_rgb(0x4c, 0xaf, 0x72)
}

/// Query-doc (smart folder) hue (see [`kind_board`]).
pub const fn kind_query() -> egui::Color32 {
    egui::Color32::from_rgb(0x95, 0x75, 0xcd)
}

/// Plan-doc hue (the PM root container; see [`kind_board`]).
/// status: vault-graph-kind-nodes
pub const fn kind_plan() -> egui::Color32 {
    egui::Color32::from_rgb(0x3f, 0x6f, 0xb5)
}

/// Epic / list-like-kind hue, shared by the list-membership edges (see
/// [`kind_board`]). status: vault-graph-kind-nodes
pub const fn kind_epic() -> egui::Color32 {
    egui::Color32::from_rgb(0x2f, 0x9e, 0x8f)
}

/// Sprint / board-like-kind hue (see [`kind_board`]).
/// status: vault-graph-kind-nodes
pub const fn kind_sprint() -> egui::Color32 {
    egui::Color32::from_rgb(0xc7, 0x5b, 0x8d)
}

/// Story/task work-note hue — a typed LEAF: the hue marks the kind without
/// the container (square) treatment (see [`kind_board`]).
/// status: vault-graph-kind-nodes
pub const fn kind_story() -> egui::Color32 {
    egui::Color32::from_rgb(0x7f, 0x9a, 0xc9)
}

/// Spec-note hue: notes defining `[slug]` spec anchors, plus their
/// `[[spec:…]]` reference edges (see [`kind_board`]).
/// status: vault-graph-spec-edges
pub const fn kind_spec() -> egui::Color32 {
    egui::Color32::from_rgb(0x5b, 0x8a, 0xa6)
}

#[cfg(test)]
mod open_signal_wash_tests {
    use super::{active_bg, hover_bg, open_signal_wash};

    /// The active/selected state outranks plain hover — a hovered active row
    /// keeps the active treatment, never the lighter hover wash.
    #[test]
    fn active_beats_hover() {
        assert_eq!(open_signal_wash(true, true), Some(active_bg()));
        assert_eq!(open_signal_wash(true, false), Some(active_bg()));
    }

    /// Plain hover paints the standard hover wash; an idle row paints nothing.
    #[test]
    fn hover_paints_hover_bg_and_idle_paints_nothing() {
        assert_eq!(open_signal_wash(false, true), Some(hover_bg()));
        assert_eq!(open_signal_wash(false, false), None);
    }
}
