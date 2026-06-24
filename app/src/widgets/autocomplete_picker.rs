//! Reusable popup picker for STANDALONE (non-buffer) autocomplete surfaces:
//! a query text field plus a ranked, keyboard-navigable list (Up/Down move,
//! Enter selects, Esc cancels), rendered as an overlay. Returns the chosen
//! [`CompletionItem`]. The in-buffer surfaces keep their inline anchored
//! popup; this is for surfaces with no text buffer (the canvas insert
//! picker, a future command-style picker).
//!
//! ## Contract
//!
//! The caller owns a [`PickerState`] (typically on its panel/app state) and
//! calls [`show`] each frame while the picker should be visible:
//!
//! ```ignore
//! // open it (e.g. from a toolbar action):
//! self.picker.open();
//!
//! // each frame, while open:
//! if self.picker.is_open() {
//!     match autocomplete_picker::show(ui, &mut self.picker, &vault_source) {
//!         PickerOutcome::Selected(item) => { /* use item.insert / item.label */ }
//!         PickerOutcome::Cancelled => { /* picker closed itself */ }
//!         PickerOutcome::Open => { /* still browsing; do nothing */ }
//!     }
//! }
//! ```
//!
//! `show` closes the state on Selected / Cancelled (so `is_open()` returns
//! `false` next frame); the caller does not have to clear it. The candidate
//! source is queried every frame with the current query — keep enumeration
//! cheap (or cache upstream). The result cap is fixed at [`MAX_RESULTS`].
//!
//! status: autocomplete-picker-widget

use eframe::egui;
use editor_view::autocomplete::CandidateSource;
use editor_view::autocomplete::CompletionItem;

/// Maximum number of ranked rows the picker shows at once.
const MAX_RESULTS: usize = 12;

/// Caller-owned state for one standalone picker. Construct with
/// [`Default::default`]; drive open/close via [`PickerState::open`] and
/// [`PickerState::is_open`].
#[derive(Default)]
pub struct PickerState {
    open: bool,
    /// Live query string bound to the text field.
    query: String,
    /// Highlighted row index into the *current* ranked list.
    selected: usize,
    /// Set once after opening so the text field grabs focus on the first
    /// frame without stealing it back every frame.
    focus_requested: bool,
}

impl PickerState {
    /// Open the picker, clearing any prior query/selection.
    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
        self.focus_requested = false;
    }

    /// Close the picker (clears nothing visible — re-`open` resets state).
    pub const fn close(&mut self) {
        self.open = false;
    }

    /// Whether the picker is currently visible.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }
}

/// What [`show`] resolved to this frame.
pub enum PickerOutcome {
    /// The user committed `item` (Enter or click). The picker is now closed.
    Selected(CompletionItem),
    /// The user dismissed the picker (Esc or clicked away). Now closed.
    Cancelled,
    /// Still browsing; nothing committed this frame.
    Open,
}

/// Render the picker for this frame and resolve any user action. Queries
/// `source` with the current query, ranks (the source ranks internally via
/// the shared core), and shows a centered popup window. See the module docs
/// for the calling contract.
pub fn show(
    ui: &mut egui::Ui,
    state: &mut PickerState,
    source: &dyn CandidateSource,
    title: &str,
) -> PickerOutcome {
    if !state.open {
        return PickerOutcome::Open;
    }
    let items = source.candidates(&state.query, MAX_RESULTS);
    if state.selected >= items.len() {
        state.selected = items.len().saturating_sub(1);
    }

    let mut items = items;
    let mut outcome = Outcome::None;
    let mut open = true;
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 80.0))
        .open(&mut open)
        .show(ui.ctx(), |ui| {
            render_body(ui, state, &items, &mut outcome);
        });

    // X-close (window's own button) counts as cancel.
    if matches!(outcome, Outcome::None) && !open {
        outcome = Outcome::Cancel;
    }
    match outcome {
        Outcome::Commit(idx) if idx < items.len() => {
            state.close();
            PickerOutcome::Selected(items.swap_remove(idx))
        }
        Outcome::Commit(_) | Outcome::Cancel => {
            state.close();
            PickerOutcome::Cancelled
        }
        Outcome::None => PickerOutcome::Open,
    }
}

/// The raw decision collected inside the window closure, resolved into a
/// [`PickerOutcome`] by [`show`].
enum Outcome {
    None,
    Cancel,
    Commit(usize),
}

/// Paint the query field + ranked list and fold key/click input into
/// `outcome`. Split out of [`show`] to keep each function small.
fn render_body(
    ui: &mut egui::Ui,
    state: &mut PickerState,
    items: &[CompletionItem],
    outcome: &mut Outcome,
) {
    ui.set_min_width(320.0);
    let field = ui.add(
        egui::TextEdit::singleline(&mut state.query)
            .hint_text("Type to filter…")
            .desired_width(f32::INFINITY),
    );
    if !state.focus_requested {
        field.request_focus();
        state.focus_requested = true;
    }

    handle_keys(ui, state, items, outcome);

    ui.add_space(4.0);
    egui::ScrollArea::vertical()
        .max_height(280.0)
        .show(ui, |ui| {
            for (idx, item) in items.iter().enumerate() {
                if render_row(ui, item, idx == state.selected) {
                    *outcome = Outcome::Commit(idx);
                }
            }
            if items.is_empty() {
                ui.weak("No matches");
            }
        });
}

/// One selectable list row: label, dimmed detail. Returns `true` on click.
fn render_row(ui: &mut egui::Ui, item: &CompletionItem, selected: bool) -> bool {
    let text = match &item.detail {
        Some(detail) if detail.as_str() != item.label.as_str() => {
            format!("{}  —  {}", item.label, detail)
        }
        _ => item.label.to_string(),
    };
    ui.selectable_label(selected, text).clicked()
}

/// Up/Down move the selection (wrapping), Enter commits, Esc cancels.
fn handle_keys(
    ui: &egui::Ui,
    state: &mut PickerState,
    items: &[CompletionItem],
    outcome: &mut Outcome,
) {
    let (down, up, enter, esc) = ui.input(|i| {
        (
            i.key_pressed(egui::Key::ArrowDown),
            i.key_pressed(egui::Key::ArrowUp),
            i.key_pressed(egui::Key::Enter),
            i.key_pressed(egui::Key::Escape),
        )
    });
    if esc {
        *outcome = Outcome::Cancel;
        return;
    }
    if !items.is_empty() {
        if down {
            state.selected = (state.selected + 1) % items.len();
        }
        if up {
            state.selected = (state.selected + items.len() - 1) % items.len();
        }
        if enter {
            *outcome = Outcome::Commit(state.selected);
        }
    }
}
