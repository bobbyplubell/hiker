//! Ctrl+K command palette. Renders a centered modal window listing every
//! registered `Action`, filtered by the user's query. Up/Down navigate,
//! Enter runs, Esc closes. The palette is independent of the toolbar
//! system — it consumes the same `ActionRegistry`, nothing more.

use eframe::egui;

use crate::actions::{Action, ActionRegistry};
use crate::state::AppState;
use crate::theme;

/// Render the palette if `app.ui.palette_open`. Call AFTER toolbars +
/// panels so the modal sits on top.
pub fn show(ctx: &egui::Context, app: &mut AppState) {
    if !app.ui.palette_open {
        return;
    }

    // Esc closes (consume so the editor doesn't see it).
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        app.ui.palette_open = false;
        return;
    }

    let matches = filter_actions(&app.ui.palette_query);
    let count = matches.len();
    if count == 0 {
        app.ui.palette_selected = 0;
    } else if app.ui.palette_selected >= count {
        app.ui.palette_selected = count - 1;
    }

    // Arrow navigation (consume so it doesn't drive the editor).
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown))
        && count > 0
    {
        app.ui.palette_selected = (app.ui.palette_selected + 1) % count;
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp))
        && count > 0
    {
        app.ui.palette_selected = if app.ui.palette_selected == 0 {
            count - 1
        } else {
            app.ui.palette_selected - 1
        };
    }

    let mut chosen: Option<&'static str> = None;
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
        && let Some(a) = matches.get(app.ui.palette_selected)
    {
        chosen = Some(a.id);
    }

    let mut open = true;
    let screen_rect = ctx.screen_rect();
    let win_w = 520.0_f32.min(screen_rect.width() - 40.0);
    egui::Window::new("Command palette")
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .title_bar(false)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 80.0))
        .fixed_size(egui::vec2(win_w, 0.0))
        .frame(
            egui::Frame::popup(&ctx.style())
                .inner_margin(egui::Margin::symmetric(10, 10)),
        )
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new("Command palette")
                    .small()
                    .color(theme::muted()),
            );
            let edit = egui::TextEdit::singleline(&mut app.ui.palette_query)
                .hint_text("Type to filter actions, Enter to run, Esc to close")
                .desired_width(f32::INFINITY);
            let resp = ui.add(edit);
            resp.request_focus();
            ui.separator();

            egui::ScrollArea::vertical()
                .max_height(360.0)
                .show(ui, |ui| {
                    render_results(ui, &matches, app.ui.palette_selected, &mut chosen);
                });
        });

    if !open {
        app.ui.palette_open = false;
    }
    if let Some(id) = chosen {
        app.ui.palette_open = false;
        crate::actions::dispatch(app, id);
    }
}

fn render_results(
    ui: &mut egui::Ui,
    matches: &[&'static Action],
    selected: usize,
    chosen: &mut Option<&'static str>,
) {
    let mut last_cat: Option<crate::actions::ActionCategory> = None;
    for (i, a) in matches.iter().enumerate() {
        if last_cat != Some(a.category) {
            if last_cat.is_some() {
                ui.add_space(2.0);
            }
            ui.label(
                egui::RichText::new(a.category.label())
                    .small()
                    .color(theme::muted()),
            );
            last_cat = Some(a.category);
        }
        let is_sel = i == selected;
        let row = ui.scope(|ui| {
            if is_sel {
                let visuals = ui.visuals_mut();
                visuals.widgets.inactive.weak_bg_fill = visuals.selection.bg_fill;
            }
            ui.horizontal(|ui| {
                ui.add((a.icon)());
                ui.label(a.label);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(a.id).small().color(theme::muted()),
                    );
                });
            })
            .response
        });
        let clickable = row.inner.interact(egui::Sense::click());
        if clickable.clicked() {
            *chosen = Some(a.id);
        }
    }
}

/// Subsequence match against label and id. Case-insensitive. Empty
/// query matches everything (in registry order).
fn filter_actions(query: &str) -> Vec<&'static Action> {
    let q = query.trim().to_lowercase();
    let all = ActionRegistry::all().list();
    if q.is_empty() {
        return all.to_vec();
    }
    all.iter()
        .copied()
        .filter(|a| {
            let l = a.label.to_lowercase();
            let i = a.id.to_lowercase();
            subseq_match(&q, &l) || subseq_match(&q, &i)
        })
        .collect()
}

/// Subsequence (not substring) match: every character of `needle` must
/// appear in `hay` in order, but not necessarily contiguously. Cheap
/// fuzzy filter that handles typos less gracefully than full fuzzy
/// matchers but is dependency-free.
fn subseq_match(needle: &str, hay: &str) -> bool {
    let mut chars = hay.chars();
    'outer: for nc in needle.chars() {
        for hc in chars.by_ref() {
            if hc == nc {
                continue 'outer;
            }
        }
        return false;
    }
    true
}
