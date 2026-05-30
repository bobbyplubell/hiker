//! Side bar — host for activity content. Implements `SPEC.md` §2/§3.

use std::hash::Hash;

use egui::{Frame, Layout};

use crate::behavior::Host;
use crate::tab::Document;
use crate::theme::Palette;

/// Which edge a side bar lives on. Default `Left`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Side {
    #[default]
    Left,
    Right,
}

/// One side bar instance. The workbench owns two of these: a primary
/// and an optional secondary (rendered on the opposite side).
pub struct SideBar {
    pub side: Side,
    pub visible: bool,
    pub width: f32,
    /// Lower bound on the user-resizable width.
    pub min_width: f32,
    /// Upper bound on the user-resizable width.
    pub max_width: f32,
}

impl Default for SideBar {
    fn default() -> Self {
        Self {
            side: Side::Left,
            visible: true,
            width: 260.0,
            min_width: 80.0,
            max_width: 600.0,
        }
    }
}

impl SideBar {
    pub fn new(side: Side) -> Self {
        Self { side, ..Self::default() }
    }

    pub const fn toggle(&mut self) {
        self.visible = !self.visible;
    }
}

/// Which side bar role is being rendered. The primary side bar's
/// content is driven by the activity bar's active mode (so
/// [`Host::side_bar_ui`] gets called). The secondary side
/// bar has fixed host content via
/// [`Host::secondary_side_bar_ui`] — it's not coupled to
/// the active activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SideBarRole {
    Primary,
    Secondary,
}

/// Render a side bar. The caller must have ensured this side bar's
/// side matches the SidePanel side it's being shown in.
///
/// The secondary bar shows the host's fixed content. (The primary bar
/// is rendered separately by the workbench through the accordion
/// `side_panel_stack`, which hosts one or more collapsible feature
/// sections.) [feature-multi-region-sidebar]
pub(crate) fn show_side_bar<Tab, Mode, B>(
    bar: &mut SideBar,
    ctx: &egui::Context,
    panel_id: impl Into<egui::Id>,
    theme: &Palette,
    behavior: &mut B,
    active_mode: Option<&Mode>,
    role: SideBarRole,
) where
    Tab: Document,
    Mode: Clone + Eq + Hash + 'static,
    B: Host<Tab, Mode> + ?Sized,
{
    if !bar.visible {
        return;
    }
    let id = panel_id.into();
    let frame = Frame::side_top_panel(&ctx.style()).fill(theme.side_bar_bg);
    let panel = match bar.side {
        Side::Left => egui::SidePanel::left(id),
        Side::Right => egui::SidePanel::right(id),
    };

    let clamped = bar.width.clamp(bar.min_width, bar.max_width);

    let response = panel
        .frame(frame)
        .resizable(true)
        .default_width(clamped)
        .min_width(bar.min_width)
        .max_width(bar.max_width)
        .show(ctx, |ui| {
            render_region::<Tab, Mode, B>(ui, behavior, active_mode, role);
        });

    let actual = response.response.rect.width();
    let new_width = actual.clamp(bar.min_width, bar.max_width);
    if (new_width - bar.width).abs() > 0.5 {
        bar.width = new_width;
    }
}

/// Render one region (header + body) for `mode`. `None` mode in the
/// primary role draws the "No activity selected" placeholder; in the
/// secondary role it always means "render the fixed secondary content".
/// Used by `show_side_bar` for the secondary bar's fixed content.
pub(crate) fn render_region<Tab, Mode, B>(
    ui: &mut egui::Ui,
    behavior: &mut B,
    mode: Option<&Mode>,
    role: SideBarRole,
) where
    Tab: Document,
    Mode: Clone + Eq + Hash + 'static,
    B: Host<Tab, Mode> + ?Sized,
{
    let primary_title = match (role, mode) {
        (SideBarRole::Primary, Some(m)) => Some(behavior.side_bar_title(m)),
        _ => None,
    };
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
            let menu_button = ui.button("…");
            egui::Popup::menu(&menu_button)
                .width(180.0)
                .show(|ui| match role {
                    SideBarRole::Primary => {
                        if let Some(mode) = mode {
                            behavior.side_bar_actions_menu(ui, mode);
                        }
                    }
                    SideBarRole::Secondary => {
                        behavior.secondary_side_bar_actions_menu(ui);
                    }
                });
            match role {
                SideBarRole::Primary => {
                    if let Some(mode) = mode {
                        behavior.side_bar_action_buttons(ui, mode);
                    }
                }
                SideBarRole::Secondary => {
                    behavior.secondary_side_bar_action_buttons(ui);
                }
            }
            ui.with_layout(Layout::left_to_right(egui::Align::Center), |ui| match role {
                SideBarRole::Primary => {
                    if let Some(t) = primary_title {
                        ui.label(t);
                    }
                }
                SideBarRole::Secondary => {
                    behavior.secondary_side_bar_title_ui(ui);
                }
            });
        });
    });
    ui.separator();

    // Body — see note: no outer scroll wrap so the host can reserve
    // finite height for sticky chrome and run its own inner scroll.
    let body_rect = ui.available_rect_before_wrap();
    let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(body_rect));
    body_ui.set_clip_rect(body_rect);
    match role {
        SideBarRole::Primary => {
            if let Some(mode) = mode {
                behavior.side_bar_ui(&mut body_ui, mode);
            } else {
                body_ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("No activity selected").weak());
                });
            }
        }
        SideBarRole::Secondary => {
            behavior.secondary_side_bar_ui(&mut body_ui);
        }
    }
    ui.advance_cursor_after_rect(body_rect);
}
