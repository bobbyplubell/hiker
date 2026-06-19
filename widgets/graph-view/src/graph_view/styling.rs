//! Visual styling for a graph view: the configurable [`Style`] (colors +
//! sizes) with its per-node [`Palette`] variants, the hover/selection
//! [`HighlightStyle`], the policy-color legend row, and the color-picker /
//! palette rows the view-options menu embeds.

use hiker_theme as theme;

/// Configurable colors + sizes for a graph view. The [`Palette`] varies the
/// per-node coloring controls (flat vault fill + active accent vs. the
/// cluster color-by-policy set); every other control is common to both.
#[derive(Clone, Copy)]
pub struct Style {
    pub edge_color: egui::Color32,
    pub label_color: egui::Color32,
    /// `None` follows the theme's `extreme_bg_color`.
    pub background: Option<egui::Color32>,
    /// Multiplier on each node's base radius.
    pub node_scale: f32,
    pub edge_width: f32,
    pub label_size: f32,
    pub palette: Palette,
    /// Optional translucent pill painted behind each label so text stays legible
    /// over a busy background / at low LOD. `None` = no background (default).
    pub label_bg: Option<egui::Color32>,
}

/// The per-node color scheme, which differs between the two graphs.
#[derive(Clone, Copy)]
pub enum Palette {
    /// Vault graph: one flat fill + an accent for the active note.
    Flat {
        node: egui::Color32,
        active: egui::Color32,
    },
    /// Cluster graph: color by node kind / policy, blended toward `stale` by
    /// summary churn.
    Policy {
        cluster: egui::Color32,
        move_policy: egui::Color32,
        tag_policy: egui::Color32,
        leaf: egui::Color32,
        stale: egui::Color32,
    },
}

impl Style {
    /// Vault-graph defaults: flat `#6b7280` nodes, active note in accent,
    /// translucent grey edges. Defaults mirror the historical hard-coded
    /// render values so an untouched graph looks unchanged.
    pub const fn flat() -> Self {
        Self {
            edge_color: egui::Color32::from_rgba_premultiplied(0x90, 0x96, 0xa0, 0xa0),
            label_color: theme::muted(),
            background: None,
            node_scale: 1.0,
            edge_width: 1.0,
            label_size: 11.0,
            palette: Palette::Flat {
                node: egui::Color32::from_rgb(0x6b, 0x72, 0x80),
                active: theme::accent(),
            },
            label_bg: None,
        }
    }

    /// Cluster-graph defaults: color-by-policy with the spec's four encoding
    /// colors plus a staleness grey, divider-colored edges.
    pub const fn policy() -> Self {
        Self {
            edge_color: theme::divider(),
            label_color: theme::muted(),
            background: None,
            node_scale: 1.0,
            edge_width: 1.0,
            label_size: 11.0,
            palette: Palette::Policy {
                cluster: theme::accent(),
                move_policy: egui::Color32::from_rgb(0x2f, 0x6f, 0xb9),
                tag_policy: egui::Color32::from_rgb(0xa8, 0x4a, 0xc4),
                leaf: egui::Color32::from_rgb(0x2f, 0x8f, 0x4d),
                stale: egui::Color32::from_rgb(0xa0, 0xa0, 0xa0),
            },
            label_bg: None,
        }
    }
}

/// Hover / selection edge-highlight appearance + toggles. Highlighting a node's
/// incident edges is a Painter overlay (see
/// [`State::draw_highlight_edges`](super::State)), independent of the GPU batch,
/// with a soft glow and (for hover) a fade in/out.
/// [graph-hover-highlight]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HighlightStyle {
    /// Highlight the HOVERED node's incident edges (with a fade in/out).
    pub hover_edges: bool,
    /// Persistently highlight the SELECTED node's incident edges (the host marks
    /// it via [`State::selected_node`](super::State::selected_node)) — e.g. the
    /// code view's drilled-into node.
    pub selected_edges: bool,
    /// Highlight colour (defaults to the theme accent).
    pub color: egui::Color32,
    /// Core stroke width in px.
    pub width: f32,
    /// Overall opacity (0..1).
    pub opacity: f32,
    /// Soft-glow halo amount (0 = crisp line, 1 = wide soft glow).
    pub softness: f32,
    /// Hover fade in/out duration, seconds.
    pub fade_secs: f32,
    /// Hover-FLOW duration, seconds: when the hover moves between two nodes, the
    /// glow cross-fades from the old node to the new one over this long, with a
    /// travelling pulse on any edge directly connecting them. status: graph-hover-flow
    pub flow_secs: f32,
    /// Fluid mode: instead of the discrete cross-fade, the highlight behaves like
    /// a fluid — energy injected at the hovered node diffuses through edges,
    /// drifts toward the selected node (its hop-distance field is the gravity),
    /// and decays, rendered as gradient strokes + node halos.
    /// status: graph-hover-fluid
    pub fluid: bool,
    /// Dim labels to the selection: when a node is selected, its label stays at
    /// full strength, its 1-hop neighbours' labels render semi-dimmed, and every
    /// other label dims — the selection's context pops out of the field.
    /// status: graph-label-dim
    pub dim_labels: bool,
}

impl Default for HighlightStyle {
    fn default() -> Self {
        Self {
            hover_edges: true,
            selected_edges: true,
            color: theme::accent(),
            width: 2.5,
            opacity: 0.9,
            softness: 0.5,
            fade_secs: 0.12,
            flow_secs: 0.25,
            fluid: true,
            dim_labels: true,
        }
    }
}

/// Multiply an egui colour's alpha by `factor` (clamped). Used for rim fade.
pub(super) fn fade(color: egui::Color32, factor: f32) -> egui::Color32 {
    if factor >= 1.0 {
        return color;
    }
    let a = (color.a() as f32 * factor.clamp(0.0, 1.0)).round() as u8;
    egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a)
}

/// Policy-color legend row (cluster graph only). No-op for a flat palette.
/// Reads the configured colors so the legend tracks any user edits.
pub fn policy_legend(ui: &mut egui::Ui, palette: &Palette) {
    let Palette::Policy {
        cluster,
        move_policy,
        tag_policy,
        leaf,
        ..
    } = palette
    else {
        return;
    };
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Encoding:").color(theme::muted()).small());
        legend_swatch(ui, *cluster, "cluster");
        legend_swatch(ui, *move_policy, "move policy");
        legend_swatch(ui, *tag_policy, "tag policy");
        legend_swatch(ui, *leaf, "leaf");
    });
}

/// The palette-specific color rows — flat node/active, or the five policy
/// colors.
pub(super) fn palette_rows(ui: &mut egui::Ui, palette: &mut Palette) {
    match palette {
        Palette::Flat { node, active } => {
            color_row(ui, "Nodes", node);
            color_row(ui, "Active note", active);
        }
        Palette::Policy {
            cluster,
            move_policy,
            tag_policy,
            leaf,
            stale,
        } => {
            color_row(ui, "Cluster", cluster);
            color_row(ui, "Move policy", move_policy);
            color_row(ui, "Tag policy", tag_policy);
            color_row(ui, "Leaf", leaf);
            color_row(ui, "Stale", stale);
        }
    }
}

/// One labeled color swatch row.
/// A labelled colour control that expands an INLINE picker
/// (`color_picker_color32`) rather than egui's default colour-button POPUP. The
/// popup opens on a higher layer, which the sticky view menu
/// (`CloseOnClickOutside`) reads as an outside click and dismisses — so the
/// picker would vanish the moment you reached for it. The inline picker lives in
/// the menu's own layer, so it stays put. Collapsed by default to stay compact.
/// Returns whether the colour changed this frame.
pub(super) fn color_row(ui: &mut egui::Ui, label: &str, color: &mut egui::Color32) -> bool {
    let mut changed = false;
    egui::CollapsingHeader::new(label)
        .id_salt(("graphview-color", label))
        .show(ui, |ui| {
            changed = egui::color_picker::color_picker_color32(
                ui,
                color,
                egui::color_picker::Alpha::OnlyBlend,
            );
        });
    changed
}

fn legend_swatch(ui: &mut egui::Ui, color: egui::Color32, label: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(10.0, 10.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2.0, color);
    ui.label(egui::RichText::new(label).small().color(theme::muted()));
}
