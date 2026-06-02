//! Command palette popover — the discoverability surface over the one
//! shared [`crate::actions::ActionRegistry`]. Per `command-palette`:
//! every registered action is a palette row (so everything reachable from
//! the toolbar is reachable here too); Enter / click dispatches it through
//! the same [`crate::actions::dispatch`] path the toolbar uses; the chord
//! (if any) comes from [`crate::keybinds`]; fuzzy filter on label + id;
//! per-session MRU floats recent picks up.

use eframe::egui;

use crate::actions::{self, Action, ActionRegistry};
use crate::keybinds::{self, Keybinds};
use crate::state::AppState;
use hiker_theme as theme;

impl AppState {
/// Render the palette overlay if `self.ui.palette_open`. Mirrors the
/// other window-level overlays (help, modal, profiler) — call after
/// panels so the popover layers on top of the dock area.
pub fn command_palette(&mut self, ctx: &egui::Context) {
    let app = self;
    if !app.ui.palette_open {
        return;
    }

    // Esc dismisses (consume so the editor doesn't see it).
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        app.ui.palette_open = false;
        return;
    }

    // Resolve the visible rows: every known keybind, filtered against
    // the query and the LLM gate, ranked by fuzzy match + MRU.
    let llm_enabled = app
        .vault_session
        .config
        .read()
        .map(|c| c.llm.enabled)
        .unwrap_or(false);
    let rows = Palette
        .build_rows(app, &app.ui.palette_query, &app.ui.palette_mru, llm_enabled);

    let count = rows.len();
    if count == 0 {
        app.ui.palette_selected = 0;
    } else if app.ui.palette_selected >= count {
        app.ui.palette_selected = count - 1;
    }

    // Arrow navigation. Consume so the keys don't drive the editor /
    // the focused TextEdit's caret.
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown))
        && count > 0
    {
        app.ui.palette_selected = (app.ui.palette_selected + 1) % count;
    }
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp))
        && count > 0
    {
        app.ui.palette_selected = if app.ui.palette_selected == 0 {
            count.saturating_sub(1)
        } else {
            app.ui.palette_selected - 1
        };
    }

    let mut chosen: Option<&'static str> = None;
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
        && let Some(row) = rows.get(app.ui.palette_selected)
    {
        chosen = Some(row.id);
    }

    let mut open = true;
    let screen_rect = ctx.screen_rect();
    let win_w = 560.0_f32.min(screen_rect.width() - 40.0);
    let win = egui::Window::new("Command palette")
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
                    Palette.render_results(
                        ui,
                        app,
                        &rows,
                        app.ui.palette_selected,
                        &mut chosen,
                    );
                });

            ui.separator();
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Enter")
                        .small()
                        .monospace()
                        .color(theme::muted()),
                );
                ui.label(egui::RichText::new("run").small().color(theme::muted()));
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("Esc")
                        .small()
                        .monospace()
                        .color(theme::muted()),
                );
                ui.label(egui::RichText::new("dismiss").small().color(theme::muted()));
            });
        });

    if !open {
        app.ui.palette_open = false;
    }
    // Dismiss on a press outside the popup (in addition to Esc). The
    // opening click fires on pointer *release*, so this press check never
    // closes it on the same frame it opened.
    if let Some(win) = win {
        let pressed_outside = ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| !win.response.rect.contains(p))
        });
        if pressed_outside {
            app.ui.palette_open = false;
        }
    }
    if let Some(id) = chosen {
        app.ui.palette_open = false;
        // Record MRU before dispatch (the dispatch may flip palette
        // state on its own — e.g. re-opening the palette via its own
        // id) so the order is stable regardless.
        record_mru(&mut app.ui.palette_mru, id);
        // Dispatch through the shared registry — the same path the
        // toolbar buttons use — so palette and toolbar can never diverge.
        actions::dispatch(app, id);
    }
}
}

/// One row in the palette result list. Holds enough context to render
/// without re-resolving every frame; rebuilt cheaply per paint.
struct Row {
    id: &'static str,
    label: &'static str,
    chord: &'static str,
    area: &'static str,
    dispatchable: bool,
}

/// Zero-sized helper namespace — keeps the palette's pure routines
/// (filter / build / render) grouped without polluting `AppState` with
/// methods only the popover uses.
struct Palette;

impl Palette {
    /// Assemble the visible row list for this frame: iterate the one
    /// shared [`ActionRegistry`] (so every toolbar-reachable command is
    /// also palette-reachable), filter on the LLM gate first (the spec
    /// hides AI-touching rows entirely when disabled — they don't appear
    /// greyed), then fuzzy-match the query against label + id, then
    /// re-order by MRU so recent picks float above their fuzzy rank.
    fn build_rows(
        self,
        app: &AppState,
        query: &str,
        mru: &[String],
        llm_enabled: bool,
    ) -> Vec<Row> {
        let q = query.trim().to_lowercase();
        let mut rows: Vec<Row> = ActionRegistry::all()
            .list()
            .iter()
            .filter(|a| llm_enabled || !is_action_ai_touching(a.id))
            .filter(|a| q.is_empty() || row_matches_query(a, &q))
            .map(|a| Row {
                id: a.id,
                label: a.label,
                chord: chord_for_id(a.id),
                area: keybinds::action_area_badge(a.id),
                dispatchable: a.enabled.map(|f| f(app)).unwrap_or(true),
            })
            .collect();

        // Sort: MRU position (lower = more recent = closer to top), then
        // label fuzzy score (already pre-filtered, so this just keeps a
        // stable order). `usize::MAX` for rows not in MRU.
        rows.sort_by_key(|r| {
            mru.iter()
                .position(|id| id == r.id)
                .unwrap_or(usize::MAX)
        });
        rows
    }

    /// Render the result list. Highlights the selected row and emits
    /// `chosen = Some(id)` on click.
    fn render_results(
        self,
        ui: &mut egui::Ui,
        app: &AppState,
        rows: &[Row],
        selected: usize,
        chosen: &mut Option<&'static str>,
    ) {
        if rows.is_empty() {
            ui.label(
                egui::RichText::new("No actions match")
                    .small()
                    .italics()
                    .color(theme::muted()),
            );
            return;
        }
        let _ = app;
        for (i, row) in rows.iter().enumerate() {
            let is_sel = i == selected;
            let row_resp = ui
                .scope(|ui| {
                    if is_sel {
                        let visuals = ui.visuals_mut();
                        visuals.widgets.inactive.weak_bg_fill = visuals.selection.bg_fill;
                    }
                    let muted = theme::muted();
                    let label_color = if row.dispatchable {
                        ui.visuals().text_color()
                    } else {
                        muted
                    };
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("[{}]", row.area))
                                .small()
                                .monospace()
                                .color(muted),
                        );
                        ui.label(egui::RichText::new(row.label).color(label_color));
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                let chord = if row.chord.is_empty() {
                                    "Unbound".to_string()
                                } else {
                                    row.chord.to_string()
                                };
                                ui.label(
                                    egui::RichText::new(chord)
                                        .small()
                                        .monospace()
                                        .color(muted),
                                );
                            },
                        );
                    })
                    .response
                });
            let clickable = row_resp.inner.interact(egui::Sense::click());
            if clickable.clicked() && row.dispatchable {
                *chosen = Some(row.id);
            }
        }
    }
}

/// Fuzzy match a registry action against the (lowercased) query. The
/// spec ranks label hits over id hits; we approximate by checking both
/// and returning true on either — sort order falls out of MRU plus the
/// list's natural order. Substring is enough for v1; a full fuzzy
/// matcher (`nucleo` / `fuzzy-matcher`) can drop in later via the same
/// predicate slot.
fn row_matches_query(a: &Action, q: &str) -> bool {
    let label = a.label.to_lowercase();
    let id = a.id.to_lowercase();
    subseq_match(q, &label) || subseq_match(q, &id)
}

/// First chord bound to a registry action id, or `""` if the command has
/// no keyboard binding. Multiple chords may map to one id (e.g. Ctrl-K
/// and Mod-Shift-P both open the palette); the palette shows the first.
fn chord_for_id(id: &str) -> &'static str {
    Keybinds
        .known_keybindings()
        .iter()
        .find(|k| k.id == id)
        .map(|k| k.chord)
        .unwrap_or("")
}

/// True when the action touches LLM-backed features. Per
/// `command-palette`, those rows hide entirely when `[llm] enabled =
/// false` so the palette doesn't surface dead verbs. No registry action
/// currently routes into LLM features, but the gate stays so future
/// additions just extend this match.
const fn is_action_ai_touching(_id: &str) -> bool {
    false
}

/// Push `id` to the front of `mru`, removing any earlier copy and
/// capping the list at a small size. Per-session only — never
/// persisted, matching `command-palette`'s "in-memory only" rule.
fn record_mru(mru: &mut Vec<String>, id: &str) {
    const CAP: usize = 16;
    mru.retain(|x| x != id);
    mru.insert(0, id.to_string());
    if mru.len() > CAP {
        mru.truncate(CAP);
    }
}

/// Subsequence (not substring) match: every character of `needle` must
/// appear in `hay` in order, but not necessarily contiguously. Cheap
/// fuzzy filter; same approximation the old palette used.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mru_floats_recent_to_front() {
        let mut mru = Vec::new();
        record_mru(&mut mru, "editor.save");
        record_mru(&mut mru, "editor.find");
        record_mru(&mut mru, "editor.save");
        assert_eq!(mru, vec!["editor.save".to_string(), "editor.find".to_string()]);
    }

    #[test]
    fn mru_caps_growth() {
        let mut mru = Vec::new();
        for i in 0..40 {
            record_mru(&mut mru, Box::leak(format!("a.{i}").into_boxed_str()));
        }
        assert!(mru.len() <= 16);
    }

    #[test]
    fn subseq_match_basic() {
        assert!(subseq_match("fi", "find in note"));
        assert!(subseq_match("rv", "reader view"));
        assert!(!subseq_match("xyz", "reader view"));
    }
}
