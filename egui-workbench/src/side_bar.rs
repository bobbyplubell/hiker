//! Side bar — host for activity content. Implements `SPEC.md` §2/§3.

use std::hash::Hash;

use egui::{Frame, Layout};

use crate::behavior::WorkbenchBehavior;
use crate::tab::DocumentTab;
use crate::theme::WorkbenchTheme;

/// Which edge a side bar lives on. Default `Left`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SideBarSide {
    #[default]
    Left,
    Right,
}

/// One side bar instance. The workbench owns two of these: a primary
/// and an optional secondary (rendered on the opposite side).
pub struct SideBar {
    pub side: SideBarSide,
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
            side: SideBarSide::Left,
            visible: true,
            width: 260.0,
            min_width: 80.0,
            max_width: 600.0,
        }
    }
}

impl SideBar {
    pub fn new(side: SideBarSide) -> Self {
        Self { side, ..Self::default() }
    }

    pub fn toggle(&mut self) {
        self.visible = !self.visible;
    }
}

/// Which side bar role is being rendered. The primary side bar's
/// content is driven by the activity bar's active mode (so
/// [`WorkbenchBehavior::side_bar_ui`] gets called). The secondary side
/// bar has fixed host content via
/// [`WorkbenchBehavior::secondary_side_bar_ui`] — it's not coupled to
/// the active activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SideBarRole {
    Primary,
    Secondary,
}

/// Render a side bar. The caller must have ensured this side bar's
/// side matches the SidePanel side it's being shown in.
pub(crate) fn show_side_bar<Tab, Mode, B>(
    bar: &mut SideBar,
    ctx: &egui::Context,
    panel_id: impl Into<egui::Id>,
    theme: &WorkbenchTheme,
    behavior: &mut B,
    active_mode: Option<&Mode>,
    role: SideBarRole,
) where
    Tab: DocumentTab,
    Mode: Clone + Eq + Hash + 'static,
    B: WorkbenchBehavior<Tab, Mode> + ?Sized,
{
    if !bar.visible {
        return;
    }
    let id = panel_id.into();
    let frame = Frame::side_top_panel(&ctx.style()).fill(theme.side_bar_bg);
    let panel = match bar.side {
        SideBarSide::Left => egui::SidePanel::left(id),
        SideBarSide::Right => egui::SidePanel::right(id),
    };

    let clamped = bar.width.clamp(bar.min_width, bar.max_width);

    let response = panel
        .frame(frame)
        .resizable(true)
        .default_width(clamped)
        .min_width(bar.min_width)
        .max_width(bar.max_width)
        .show(ctx, |ui| {
            // Header row. Render via a single right-to-left layout so
            // the actions menu sits flush against the right edge and
            // the title takes whatever space is left to the left. The
            // earlier two-`with_layout` approach split the row into
            // two regions that each padded themselves and pushed
            // content to the far right of the panel.
            let primary_title = match (role, active_mode) {
                (SideBarRole::Primary, Some(m)) => Some(behavior.side_bar_title(m)),
                _ => None,
            };
            ui.add_space(2.0);
            // Wrap the header in `ui.horizontal` so it consumes one row
            // of height — without it, the inner `with_layout(...)`
            // sizes its child UI to the panel's full remaining height,
            // and `Align::Center` would float the header content
            // vertically while starving the body of space.
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                    let menu_button = ui.button("…");
                    // The "…" lives inside a `right_to_left` layout
                    // nested in a `horizontal` row. Without an explicit
                    // width hint, `Popup::menu` inherits a near-zero
                    // wrap width from that context and each menu label
                    // collapses onto its own line, word by word. A
                    // 180px floor lets standard "Sort by …" / "Refresh
                    // tree" entries lay out on a single row.
                    egui::Popup::menu(&menu_button)
                        .width(180.0)
                        .show(|ui| match role {
                            SideBarRole::Primary => {
                                if let Some(mode) = active_mode {
                                    behavior.side_bar_actions_menu(ui, mode);
                                }
                            }
                            SideBarRole::Secondary => {
                                behavior.secondary_side_bar_actions_menu(ui);
                            }
                        });
                    match role {
                        SideBarRole::Primary => {
                            if let Some(mode) = active_mode {
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

            // Body — drive the host. Primary is activity-driven; the
            // secondary bar's content is fixed regardless of which
            // activity is active.
            //
            // We deliberately do NOT wrap the host's body in a scroll
            // area. Hosts often need finite `ui.available_height()` to
            // reserve space for sticky chrome (e.g., a trash bin row
            // pinned at the bottom of a file panel) and run their own
            // inner scroll area for the scrollable region. An outer
            // scroll wrap would make `available_height` effectively
            // infinite and break those layouts.
            let body_rect = ui.available_rect_before_wrap();
            let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(body_rect));
            body_ui.set_clip_rect(body_rect);
            match role {
                SideBarRole::Primary => {
                    if let Some(mode) = active_mode {
                        behavior.side_bar_ui(&mut body_ui, mode);
                    } else {
                        body_ui.centered_and_justified(|ui| {
                            ui.label(
                                egui::RichText::new("No activity selected").weak(),
                            );
                        });
                    }
                }
                SideBarRole::Secondary => {
                    behavior.secondary_side_bar_ui(&mut body_ui);
                }
            }
            ui.advance_cursor_after_rect(body_rect);
        });

    let actual = response.response.rect.width();
    let new_width = actual.clamp(bar.min_width, bar.max_width);
    if (new_width - bar.width).abs() > 0.5 {
        bar.width = new_width;
    }
}
