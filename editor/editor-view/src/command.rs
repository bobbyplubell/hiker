//! Default command set. Translates [`InputEvent`]s into [`Transaction`]s and
//! direct selection / scroll mutations on the [`ViewState`].

use editor_core::change::Set as ChangeSet;
use editor_core::decoration::Decoration;
use editor_core::transaction::EditType;

use editor_core::state::Editor as EditorState;
use editor_core::selection::SelRange;

use editor_core::selection::Selection;

use editor_core::transaction::Transaction;
use smol_str::SmolStr;

use crate::autocomplete::{CompletionItem, CompletionKind};
use crate::snippets::{self, Snippet};
use crate::events::{
    ImeEvent, InputEvent, Key, KeyEvent, Modifiers, MouseButton, MouseEvent, NamedKey,
};
use crate::motion::{self, Direction};
use crate::multicursor;
use crate::viewport::{ClickAction, DragState, ViewState};

/// Outcome of handling one input event.
pub enum Action {
    /// Replace the editor state with the given new state.
    ///
    /// `tx` carries the change set that produced `state`, when one was built
    /// from user input — the forward half of the editor binding the host can
    /// mirror into a higher layer. Doc-mutating arms set `Some(tx)`;
    /// selection-only arms (and history navigation like undo/redo, where no
    /// fresh change set was authored) set `None`. The host treats `tx` as an
    /// optional side channel: applying `state` is unconditional, emitting `tx`
    /// is opt-in.
    Replace {
        state: EditorState,
        tx: Option<Transaction>,
    },
    /// Just touch the view (scroll changed, drag updated, etc.).
    None,
    /// Request a clipboard write.
    Copy(String),
    /// Cut: write `text` to the clipboard *and* replace the editor with
    /// the post-delete `state`. A single `Action` can only carry one
    /// outcome, so cut — which both copies and edits — needs its own
    /// variant rather than being forced to choose between `Copy` and
    /// `Replace`. The host consumes all three: clipboard write, state swap,
    /// and `tx` (the deletion's change set, mirrored into the binding's
    /// `working` layer just like a `Replace` tx — without it the cut would
    /// only touch `editor.doc` and get reverted on the next reverse pass).
    Cut { text: String, state: EditorState, tx: Transaction },
    /// Click landed on a clickable decoration zone (e.g. an Expander).
    Click(ClickAction),
}

impl Action {
    /// Doc-mutating replace: a freshly-built change set produced `state`.
    /// `tx` rides along so the host can mirror the applied edit.
    const fn doc(state: EditorState, tx: Transaction) -> Self {
        Self::Replace { state, tx: Some(tx) }
    }

    /// Replace that carries no emittable change set — selection-only edits and
    /// history navigation (undo / redo), where the new state didn't come from a
    /// user-authored change set this handler built.
    const fn state_only(state: EditorState) -> Self {
        Self::Replace { state, tx: None }
    }

    /// True for actions that changed the document text — the cases where the
    /// caret should be scrolled back into view. Selection-only replaces
    /// (`tx: None`, incl. undo/redo and pure motion) and clipboard copies
    /// don't qualify; this scopes caret-follow to edits, per
    /// `bug-editor-no-scroll-cursor-into-view-on-edit`.
    const fn mutates_doc(&self) -> bool {
        matches!(self, Self::Replace { tx: Some(_), .. } | Self::Cut { .. })
    }
}

/// Command-handler context: bundles the immutable editor state and mutable
/// view state that every command needs. Sub-handlers (key/mouse dispatch,
/// motion, snippet cycling, etc.) live as methods on this struct so that the
/// top-level dispatcher stays under the cognitive-complexity budget while
/// each helper still gets a unique entry point.
struct Cmd<'a> {
    state: &'a EditorState,
    view: &'a mut ViewState,
}

pub fn handle(state: &EditorState, view: &mut ViewState, event: &InputEvent) -> Action {
    let action = dispatch_event(state, view, event);
    // An edit moved the caret — request a scroll-into-view, applied by the
    // widget after its next measure pass. status: scroll-caret-into-view-on-edit
    if action.mutates_doc() {
        view.scroll_caret_into_view = true;
    }
    action
}

fn dispatch_event(state: &EditorState, view: &mut ViewState, event: &InputEvent) -> Action {
    if view.read_only {
        return match event {
            InputEvent::Mouse(ev) => handle_mouse(state, view, ev),
            InputEvent::Scroll { delta_y, .. } => {
                scroll_by(view, *delta_y);
                Action::None
            }
            InputEvent::Copy => copy_selection(state),
            InputEvent::Focus(_) => Action::None,
            InputEvent::Key(KeyEvent { key, mods, .. }) => {
                // Allow read-only motion/copy/select-all.
                match key {
                    Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowRight | NamedKey::ArrowUp
                        | NamedKey::ArrowDown | NamedKey::Home | NamedKey::End
                        | NamedKey::PageUp | NamedKey::PageDown | NamedKey::Escape) => {
                        handle_key(state, view, *key, *mods)
                    }
                    Key::Char('a' | 'A' | 'c' | 'C') if mods.primary_only() => {
                        handle_key(state, view, *key, *mods)
                    }
                    _ => Action::None,
                }
            }
            _ => Action::None,
        };
    }
    match event {
        InputEvent::Text(s) => insert_text(state, view, s),
        InputEvent::Key(KeyEvent { key, mods, .. }) => handle_key(state, view, *key, *mods),
        InputEvent::Ime(ev) => match ev {
            ImeEvent::Enabled => {
                view.ime.enabled = true;
                Action::None
            }
            ImeEvent::Disabled => {
                view.ime.enabled = false;
                view.ime.clear_preedit();
                Action::None
            }
            ImeEvent::Preedit(text) => {
                view.ime.preedit = if text.is_empty() { None } else { Some(text.clone()) };
                Action::None
            }
            ImeEvent::Commit(text) => {
                view.ime.clear_preedit();
                if text.is_empty() {
                    Action::None
                } else {
                    let tx = state.insert_at_selections(text);
                    let new_state = state.apply(tx.clone());
                    Action::doc(new_state, tx)
                }
            }
        },
        InputEvent::Mouse(ev) => handle_mouse(state, view, ev),
        InputEvent::Scroll { delta_y, .. } => {
            scroll_by(view, *delta_y);
            Action::None
        }
        InputEvent::Focus(_) => Action::None,
        InputEvent::Paste(s) => {
            // Let a language provider rewrite the pasted text first — e.g. the
            // markdown indenter strips a leading `- ` bullet when the caret
            // already sits right after a list-item bullet, so the buffer's
            // existing marker isn't doubled.
            if let Some(provider) = view.indent_provider.clone() {
                if let Some(rewritten) = provider.on_paste(state, s) {
                    return insert_text(state, view, &rewritten);
                }
            }
            insert_text(state, view, s)
        }
        InputEvent::Copy => copy_selection(state),
        InputEvent::Cut => {
            // Reuse the exact text `copy_selection` would put on the
            // clipboard (selection, or whole line when empty), then delete
            // *those same bytes*. Cut and copy share `cut_range` so the line
            // copied on an empty selection is also the line deleted (a plain
            // `delete_at_selections` would only backspace one char). Both
            // halves ride out on `Action::Cut` so the clipboard write isn't
            // dropped — the bug this fixes.
            if let Action::Copy(text) = copy_selection(state) {
                let ranges: Vec<_> =
                    state.selection.ranges().iter().map(|r| cut_range(state, r)).collect();
                let tx = state.delete_ranges(&ranges);
                let new_state = state.apply(tx.clone());
                view.touch();
                return Action::Cut { text, state: new_state, tx };
            }
            Action::None
        }
    }
}

/// Apply a transaction, then — if a snippet is active — map the snippet's
/// anchors through the change and run a mirror sync so the primary cursor's
/// text is propagated to every mirror span. Returns the final state.
fn apply_with_snippet(state: &EditorState, view: &mut ViewState, tx: Transaction) -> EditorState {
    let changes = tx.changes.clone();
    let after = state.apply(tx);
    if view.snippet.is_active() {
        snippets::map_through(&mut view.snippet, &changes);
        if let Some(sync_tx) = snippets::mirror_sync(&after, &view.snippet) {
            let sync_changes = sync_tx.changes.clone();
            let synced = after.apply(sync_tx);
            snippets::map_through(&mut view.snippet, &sync_changes);
            return synced;
        }
    }
    after
}

impl<'a> Cmd<'a> {

/// Motion-only key dispatch. Extracted from the top-level `handle_key` to
/// keep the dispatcher's cognitive complexity under the clippy budget.
/// Returns `Some(action)` if the key was a motion key (arrows / page /
/// home / end / column-cursor add), `None` otherwise.
fn handle_motion_key(
    &mut self,
    key: Key,
    mods: Modifiers,
    extend: bool,
    word_jump: bool,
) -> Option<Action> {
    let state = self.state;
    let view = &mut *self.view;
    use Direction::*;
    let action = match key {
        Key::Named(NamedKey::ArrowUp) if mods.alt && mods.primary() => {
            let sel = multicursor::add_vertical_cursor(state, false);
            Action::state_only(apply_selection(state, sel))
        }
        Key::Named(NamedKey::ArrowDown) if mods.alt && mods.primary() => {
            let sel = multicursor::add_vertical_cursor(state, true);
            Action::state_only(apply_selection(state, sel))
        }
        Key::Named(NamedKey::ArrowLeft) => {
            let layers = view.decorations.layers.as_slice();
            let sel = if word_jump {
                motion::move_word(state, Left, extend, layers)
            } else {
                motion::move_char(state, Left, extend, layers)
            };
            Action::state_only(apply_selection(state, sel))
        }
        Key::Named(NamedKey::ArrowRight) => {
            let layers = view.decorations.layers.as_slice();
            let sel = if word_jump {
                motion::move_word(state, Right, extend, layers)
            } else {
                motion::move_char(state, Right, extend, layers)
            };
            Action::state_only(apply_selection(state, sel))
        }
        Key::Named(NamedKey::ArrowUp) => {
            let wrap = if view.wrap_map.enabled() { Some(&view.wrap_map) } else { None };
            let sel = motion::move_vertical_wrapped(state, Up, extend, 1, wrap);
            Action::state_only(apply_selection(state, sel))
        }
        Key::Named(NamedKey::ArrowDown) => {
            let wrap = if view.wrap_map.enabled() { Some(&view.wrap_map) } else { None };
            let sel = motion::move_vertical_wrapped(state, Down, extend, 1, wrap);
            Action::state_only(apply_selection(state, sel))
        }
        Key::Named(NamedKey::PageUp) => {
            let lines = ((view.height / view.line_height).floor() as usize).max(1);
            let wrap = if view.wrap_map.enabled() { Some(&view.wrap_map) } else { None };
            let sel = motion::move_vertical_wrapped(state, Up, extend, lines, wrap);
            Action::state_only(apply_selection(state, sel))
        }
        Key::Named(NamedKey::PageDown) => {
            let lines = ((view.height / view.line_height).floor() as usize).max(1);
            let wrap = if view.wrap_map.enabled() { Some(&view.wrap_map) } else { None };
            let sel = motion::move_vertical_wrapped(state, Down, extend, lines, wrap);
            Action::state_only(apply_selection(state, sel))
        }
        Key::Named(NamedKey::Home) => {
            let sel = if mods.primary() {
                motion::move_doc_edge(state, false, extend)
            } else {
                motion::move_line_edge(state, false, extend)
            };
            Action::state_only(apply_selection(state, sel))
        }
        Key::Named(NamedKey::End) => {
            let sel = if mods.primary() {
                motion::move_doc_edge(state, true, extend)
            } else {
                motion::move_line_edge(state, true, extend)
            };
            Action::state_only(apply_selection(state, sel))
        }
        _ => return None,
    };
    Some(action)
}

}

fn handle_key(state: &EditorState, view: &mut ViewState, key: Key, mods: Modifiers) -> Action {
    view.touch();
    view.ime.clear_preedit();
    // Any key event invalidates a pending auto-pair skip — the user moved on.
    view.autopair_skip_at = None;

    // Search panel keybindings. Cmd-F / Ctrl-F always opens. While the panel
    // is active, Enter / Shift-Enter / Escape are intercepted for match
    // navigation and dismissal BEFORE any other handler.
    if let Some(action) = (Cmd { state, view }).handle_search_key(key, mods) {
        return action;
    }

    // Snippet cycling: intercept Tab / Shift-Tab / Escape while a snippet
    // expansion is active. Must run BEFORE the existing Tab indent path
    // and before completion handling so the user's Tab advances the stop.
    if view.snippet.is_active() {
        if let Some(action) = (Cmd { state, view }).handle_snippet_key(key, mods) {
            return action;
        }
    }

    if view.completion.active {
        if let Some(action) = (Cmd { state, view }).handle_completion_key(key, mods) {
            return action;
        }
    }

    let extend = mods.shift;

    // Word-granularity if alt (mac) or ctrl (non-mac). egui maps OS primary to `meta`
    // on mac and `ctrl` on Linux/Windows; word-jump is alt on mac, ctrl on linux/win.
    // We use `alt` here for word boundaries — most platforms accept it.
    let word_jump = mods.alt;

    if let Some(action) = (Cmd { state, view }).handle_motion_key(key, mods, extend, word_jump) {
        return action;
    }

    match key {
        Key::Named(NamedKey::Backspace) => {
            let tx = backspace_outdent(state).unwrap_or_else(|| state.delete_at_selections());
            Action::doc(apply_with_snippet(state, view, tx.clone()), tx)
        }
        Key::Named(NamedKey::Delete) => {
            let tx = (Cmd { state, view }).delete_forward();
            Action::doc(apply_with_snippet(state, view, tx.clone()), tx)
        }
        Key::Named(NamedKey::Enter) if mods.is_empty() => {
            if let Some(provider) = view.indent_provider.clone() {
                if let Some(tx) = provider.on_enter(state) {
                    return Action::doc(state.apply(tx.clone()), tx);
                }
            }
            insert_text(state, view, "\n")
        }
        Key::Named(NamedKey::Enter) if mods.shift => insert_text(state, view, "\n"),
        Key::Named(NamedKey::Tab) if mods.is_empty() => {
            if let Some(provider) = view.indent_provider.clone() {
                if let Some(tx) = provider.on_tab(state) {
                    return Action::doc(state.apply(tx.clone()), tx);
                }
            }
            (Cmd { state, view }).indent_tab()
        }
        Key::Named(NamedKey::Tab) if mods.shift && !mods.primary() && !mods.alt => {
            if let Some(provider) = view.indent_provider.clone() {
                if let Some(tx) = provider.on_shift_tab(state) {
                    return Action::doc(state.apply(tx.clone()), tx);
                }
            }
            (Cmd { state, view }).shift_tab_outdent()
        }
        // Note: don't handle plain Space here. egui emits BOTH a Key
        // event and a Text(" ") event for one physical space press; the
        // Text branch inserts the space, so handling Space here would
        // double-insert. Modifier-bearing Space chords (Ctrl-Space etc.)
        // are intercepted higher up in `app::keybinds`.
        Key::Char('a') | Key::Char('A') if mods.primary_only() => {
            let sel = motion::select_all(state);
            Action::state_only(apply_selection(state, sel))
        }
        Key::Char('z') | Key::Char('Z') if mods.primary_only() => {
            // Undo carries its inverse change set so the host binding mirrors
            // it into the `working` layer; a tx-less undo would only touch
            // `editor.doc` and get reverted on the next reverse pass.
            match state.undo_with_changes() {
                Some((next, tx)) => Action::doc(next, tx),
                None => Action::None,
            }
        }
        Key::Char('z') | Key::Char('Z') if mods.primary() && mods.shift && !mods.alt => {
            match state.redo_with_changes() {
                Some((next, tx)) => Action::doc(next, tx),
                None => Action::None,
            }
        }
        Key::Char('y') | Key::Char('Y') if mods.primary_only() => {
            match state.redo_with_changes() {
                Some((next, tx)) => Action::doc(next, tx),
                None => Action::None,
            }
        }
        Key::Char('c') | Key::Char('C') if mods.primary_only() => copy_selection(state),
        // Cmd-D / Ctrl-D — add next occurrence of selection.
        Key::Char('d') | Key::Char('D') if mods.primary_only() => {
            let sel = multicursor::add_next_occurrence(state);
            Action::state_only(apply_selection(state, sel))
        }
        // Escape collapses to the main cursor.
        Key::Named(NamedKey::Escape) => {
            let main = state.selection.main().head.offset();
            let sel = editor_core::selection::Selection::single(main);
            Action::state_only(apply_selection(state, sel))
        }
        _ => Action::None,
    }
}

fn insert_text(state: &EditorState, view: &mut ViewState, s: &str) -> Action {
    view.touch();
    view.ime.clear_preedit();
    if s.is_empty() {
        return Action::None;
    }
    let saved_skip = view.autopair_skip_at.take();

    // First: if we're typing a close char right before an auto-inserted close,
    // skip over it instead of inserting a duplicate.
    if s.chars().count() == 1
        && state.selection.ranges().iter().all(editor_core::selection::SelRange::is_empty)
    {
        if let Some(tx) = crate::pairs::autopair_skip(state, saved_skip, s) {
            return Action::doc(apply_with_snippet(state, view, tx.clone()), tx);
        }
    }

    // Auto-pair: only when typing a single char and no selection text.
    if s.chars().count() == 1
        && state.selection.ranges().iter().all(editor_core::selection::SelRange::is_empty)
    {
        if let Some(tx) = crate::pairs::autopair_transform(state, s) {
            let new_state = apply_with_snippet(state, view, tx.clone());
            // Record the skip marker: cursor is between open and close, so the
            // close char ends one char-len past the cursor.
            if let Some(first) = s.chars().next() {
                if let Some(pair) = crate::pairs::DEFAULT_PAIRS
                    .iter()
                    .find(|p| p.open == first)
                {
                    let cursor = new_state.selection.main().head.offset();
                    view.autopair_skip_at = Some(cursor + pair.close.len_utf8());
                }
            }
            maybe_open_completion(&new_state, view, s);
            return Action::doc(new_state, tx);
        }
    }
    let tx = state.insert_at_selections(s);
    let new_state = apply_with_snippet(state, view, tx.clone());
    if s.chars().count() == 1 {
        maybe_open_completion(&new_state, view, s);
    } else if view.completion.active {
        // Multi-char paste closes the popup.
        view.completion.close();
    }
    Action::doc(new_state, tx)
}

/// If `s` is a single character that any registered source advertises as a
/// trigger (or completion is already active), refresh the popup.
fn maybe_open_completion(state: &EditorState, view: &mut ViewState, s: &str) {
    let ch = match s.chars().next() {
        Some(c) if s.chars().count() == 1 => c,
        _ => return,
    };
    let pos = state.selection.main().head.offset();

    if view.completion.active {
        // Extend the query and refilter.
        view.completion.query.push(ch);
        let items = gather_matches(state, view, pos);
        if items.is_empty() {
            view.completion.close();
        } else {
            view.completion.items = items;
            view.completion.selected = 0;
        }
        return;
    }

    let triggered = view
        .completion_sources
        .iter()
        .any(|src| src.triggers().contains(&ch));
    // Re-open even on a non-trigger keystroke when the caret sits inside
    // a source's context (editing an existing `[[wikilink]]`). The cheap
    // `reopens_in_context` predicate gates the heavier `matches` walk so
    // ordinary typing doesn't pay for it. [bug-wikilink-edit-reopens-popup]
    let in_context = !triggered
        && view
            .completion_sources
            .iter()
            .any(|src| src.reopens_in_context(state, pos));
    if !triggered && !in_context {
        return;
    }
    let items = gather_matches(state, view, pos);
    if !items.is_empty() {
        view.completion.open(pos, items);
    }
}

fn gather_matches(state: &EditorState, view: &ViewState, pos: usize) -> Vec<CompletionItem> {
    let mut out = Vec::new();
    for src in &view.completion_sources {
        out.extend(src.matches(state, pos));
    }
    out
}

impl<'a> Cmd<'a> {

/// Search-panel key interception. Cmd-F / Ctrl-F opens the panel. When the
/// panel is active, Enter advances to the next match, Shift-Enter to the
/// previous, and Escape closes. Returns `Some(Action::None)` when the key was
/// consumed; `None` to fall through to other handlers.
fn handle_search_key(&mut self, key: Key, mods: Modifiers) -> Option<Action> {
    let view = &mut *self.view;
    if matches!(key, Key::Char('f') | Key::Char('F')) && mods.primary_only() {
        view.search.open();
        return Some(Action::None);
    }
    if !view.search.active {
        return None;
    }
    match key {
        Key::Named(NamedKey::Escape) if mods.is_empty() => {
            view.search.close();
            Some(Action::None)
        }
        Key::Named(NamedKey::Enter) if mods.is_empty() => {
            view.search.next();
            Some(Action::None)
        }
        Key::Named(NamedKey::Enter) if mods.shift && !mods.alt && !mods.primary() => {
            view.search.prev();
            Some(Action::None)
        }
        _ => None,
    }
}

/// Handle a key while the completion popup is open. Returns `Some(action)`
/// if the key was consumed; `None` to fall through to normal handling.
fn handle_completion_key(
    &mut self,
    key: Key,
    mods: Modifiers,
) -> Option<Action> {
    let state = self.state;
    let view = &mut *self.view;
    if !mods.is_empty() && !mods.shift {
        // Allow modifier-laden keys (shortcuts) to fall through.
        return None;
    }
    match key {
        Key::Named(NamedKey::ArrowUp) => {
            view.completion.move_selection(-1);
            Some(Action::None)
        }
        Key::Named(NamedKey::ArrowDown) => {
            view.completion.move_selection(1);
            Some(Action::None)
        }
        Key::Named(NamedKey::Escape) => {
            view.completion.close();
            Some(Action::None)
        }
        Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Tab) => {
            Some((Cmd { state, view }).commit_completion())
        }
        Key::Named(NamedKey::Backspace) => {
            if view.completion.query.is_empty() {
                view.completion.close();
                None
            } else {
                view.completion.query.pop();
                // Apply the backspace to the doc, then refilter.
                let tx = state.delete_at_selections();
                let new_state = state.apply(tx.clone());
                let pos = new_state.selection.main().head.offset();
                let items = gather_matches(&new_state, view, pos);
                if items.is_empty() {
                    view.completion.close();
                } else {
                    view.completion.items = items;
                    view.completion.selected = 0;
                }
                Some(Action::doc(new_state, tx))
            }
        }
        _ => None,
    }
}

fn commit_completion(&mut self) -> Action {
    let state = self.state;
    let view = &mut *self.view;
    let item = match view.completion.selected_item().cloned() {
        Some(it) => it,
        None => {
            view.completion.close();
            return Action::None;
        }
    };
    let pos = state.selection.main().head.offset();
    let range = item
        .replace_range
        .clone()
        .unwrap_or(view.completion.anchor_byte..pos);

    // Snippet kind: parse the insert string as a snippet template and expand.
    if item.kind == CompletionKind::Snippet {
        if let Ok(snip) = Snippet::parse(&item.insert) {
            view.completion.close();
            return expand_snippet(state, view, &snip, range);
        }
    }

    let edits = vec![(range, item.insert.to_string())];
    let changes = ChangeSet::of(state.doc.len_bytes(), edits);
    let tx = Transaction::new(changes).with_edit_type(EditType::Input);
    view.completion.close();
    Action::doc(state.apply(tx.clone()), tx)
}

}

/// Apply a snippet expansion: build the insert transaction, then set the
/// selection to the first stop's mirror spans and store the cycling state.
pub fn expand_snippet(
    state: &EditorState,
    view: &mut ViewState,
    snip: &Snippet,
    range: std::ops::Range<usize>,
) -> Action {
    let pos = range.start;
    let (tx, mut snip_state) = snip.expand(state, pos, Some(range));
    // Anchors were built against positions in the new doc, so no mapping needed.
    let after = state.apply(tx.clone());
    let sel = snippets::selection_for_stop(&snip_state, 0)
        .unwrap_or_else(|| Selection::single(pos + snip.text().len()));
    let with_sel = Transaction::new(ChangeSet::empty(after.doc.len_bytes())).with_selection(sel);
    let after = after.apply(with_sel);
    // If there is only `$0` (or no stops at all), there is nothing to cycle.
    if snip_state.stops.is_empty() {
        snip_state.cancel();
    }
    view.snippet = snip_state;
    // The doc-mutating change is the snippet insert `tx`; the trailing
    // selection set carries no content change.
    Action::doc(after, tx)
}

/// Width of one indentation step, matching what [`Cmd::indent_tab`] inserts.
const TAB_WIDTH: usize = 4;

/// Backspace over a full tab-width group of leading indentation spaces.
///
/// Returns a delete-transaction when the caret is a single empty selection
/// sitting in its line's leading indentation, that indentation is made up
/// entirely of spaces, and the run of spaces before the caret is a non-zero
/// multiple of [`TAB_WIDTH`]. In that case one whole tab-width group is
/// removed instead of a single space. Returns `None` in every other case
/// (mid-line text, a tab character in the run, a partial group, a non-empty
/// or multi-range selection) so the caller falls back to single-char delete.
fn backspace_outdent(state: &EditorState) -> Option<Transaction> {
    if state.selection.ranges().len() != 1 {
        return None;
    }
    let main = state.selection.main();
    if !main.is_empty() {
        return None;
    }
    let cursor = main.head.offset();
    let line = state.doc.byte_to_line(cursor);
    let line_start = state.doc.line_to_byte(line);
    // Bytes between the line start and the caret must all be spaces — i.e. the
    // caret sits inside the leading indentation, not after any content. A tab
    // in the run disqualifies grouping (its visual width is ambiguous).
    let prefix = &state.doc.line_str(line)[..cursor - line_start];
    if prefix.is_empty() || !prefix.bytes().all(|b| b == b' ') {
        return None;
    }
    // Only group when the whitespace run is a whole number of tab widths;
    // otherwise a single backspace lands the caret on a tab-width boundary.
    if prefix.len() % TAB_WIDTH != 0 {
        return None;
    }
    let edits = vec![(cursor - TAB_WIDTH..cursor, String::new())];
    let changes = ChangeSet::of(state.doc.len_bytes(), edits);
    Some(Transaction::new(changes).with_edit_type(EditType::Delete))
}

impl<'a> Cmd<'a> {

/// Snippet key handling. Returns `Some(action)` if the key was consumed.
fn handle_snippet_key(
    &mut self,
    key: Key,
    mods: Modifiers,
) -> Option<Action> {
    let state = self.state;
    let view = &mut *self.view;
    match key {
        Key::Named(NamedKey::Escape) if mods.is_empty() => {
            view.snippet.cancel();
            Some(Action::None)
        }
        Key::Named(NamedKey::Tab) if mods.is_empty() => {
            Some((Cmd { state, view }).advance_snippet(1))
        }
        Key::Named(NamedKey::Tab) if mods.shift && !mods.primary() && !mods.alt => {
            Some((Cmd { state, view }).advance_snippet(-1))
        }
        _ => None,
    }
}

fn advance_snippet(&mut self, delta: i32) -> Action {
    let state = self.state;
    let view = &mut *self.view;
    // First, sync any mirrors at the *current* stop into the doc before moving.
    // That sync is the only content change here; the stop-advance itself is a
    // selection-only set. Carry the sync tx so the host can mirror it.
    let mut working = state.clone();
    let synced = snippets::mirror_sync(&working, &view.snippet);
    if let Some(tx) = synced.clone() {
        let changes = tx.changes.clone();
        working = working.apply(tx);
        snippets::map_through(&mut view.snippet, &changes);
    }
    let replace = |state| Action::Replace { state, tx: synced.clone() };
    let n = view.snippet.stops.len() as i32;
    if n == 0 {
        view.snippet.cancel();
        return replace(working);
    }
    let next = view.snippet.current as i32 + delta;
    if next < 0 || next >= n {
        // Past the final stop — cancel and leave caret where the doc has it.
        view.snippet.cancel();
        return replace(working);
    }
    view.snippet.current = next as usize;
    let sel = match snippets::selection_for_stop(&view.snippet, view.snippet.current) {
        Some(s) => s,
        None => {
            view.snippet.cancel();
            return replace(working);
        }
    };
    let tx = Transaction::new(ChangeSet::empty(working.doc.len_bytes())).with_selection(sel);
    replace(working.apply(tx))
}

/// Tab: insert one tab-width of spaces at every caret. SPEC §9.14 leaves the
/// smarter "indent the entire selected block" for a future revision; the v1
/// rule is "insert a tab-width of spaces at the caret" regardless of column.
fn indent_tab(&mut self) -> Action {
    insert_text(self.state, self.view, &" ".repeat(TAB_WIDTH))
}

/// Shift-Tab: for every line that intersects the selection, remove up to 4
/// leading whitespace bytes (spaces, or a single leading tab counted as 4).
fn shift_tab_outdent(&mut self) -> Action {
    let state = self.state;
    let view = &mut *self.view;
    view.touch();
    let mut touched_lines = std::collections::BTreeSet::new();
    for r in state.selection.ranges().iter() {
        let lo = r.start();
        let hi = r.end();
        let first = state.doc.byte_to_line(lo);
        let last = state.doc.byte_to_line(hi);
        for line in first..=last {
            touched_lines.insert(line);
        }
    }
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for line in touched_lines {
        let line_start = state.doc.line_to_byte(line);
        let line_text = state.doc.line_str(line);
        let stripped = line_text.strip_suffix('\n').unwrap_or(&line_text);
        let bytes = stripped.as_bytes();
        let mut remove = 0;
        // Drop up to 4 leading spaces, OR a single leading tab.
        if !bytes.is_empty() && bytes[0] == b'\t' {
            remove = 1;
        } else {
            while remove < 4 && remove < bytes.len() && bytes[remove] == b' ' {
                remove += 1;
            }
        }
        if remove > 0 {
            edits.push((line_start..line_start + remove, String::new()));
        }
    }
    if edits.is_empty() {
        return Action::None;
    }
    edits.sort_by_key(|(r, _)| r.start);
    let changes = ChangeSet::of(state.doc.len_bytes(), edits);
    let tx = Transaction::new(changes).with_edit_type(EditType::Indent);
    Action::doc(state.apply(tx.clone()), tx)
}

fn delete_forward(&self) -> Transaction {
    let state = self.state;
    let mut edits: Vec<(std::ops::Range<usize>, String)> = state
        .selection
        .ranges()
        .iter()
        .map(|r| {
            if r.is_empty() {
                let start = r.start();
                if start == state.doc.len_bytes() {
                    (start..start, String::new())
                } else {
                    let next = state.doc.next_char_boundary(start);
                    (start..next, String::new())
                }
            } else {
                (r.range(), String::new())
            }
        })
        .collect();
    edits.sort_by_key(|(r, _)| r.start);
    edits.dedup_by_key(|(r, _)| r.clone());
    let changes = ChangeSet::of(state.doc.len_bytes(), edits);
    Transaction::new(changes).with_edit_type(EditType::Delete)
}

}

/// Byte range copied/cut for one selection range: the range itself when
/// non-empty, else the whole line including its trailing newline (VSCode
/// line-wise copy/cut with no selection). Shared by copy and cut so the two
/// always agree on which bytes are involved.
fn cut_range(state: &EditorState, r: &SelRange) -> std::ops::Range<usize> {
    if !r.is_empty() {
        return r.range();
    }
    let line = state.doc.byte_to_line(r.start());
    let start = state.doc.line_to_byte(line);
    let end = if line + 1 < state.doc.len_lines() {
        state.doc.line_to_byte(line + 1)
    } else {
        state.doc.len_bytes()
    };
    start..end
}

fn copy_selection(state: &EditorState) -> Action {
    let mut out = String::new();
    for (i, r) in state.selection.ranges().iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&state.doc.slice(cut_range(state, r)).to_string());
    }
    Action::Copy(out)
}

fn apply_selection(state: &EditorState, sel: Selection) -> EditorState {
    let tx = Transaction::new(ChangeSet::empty(state.doc.len_bytes())).with_selection(sel);
    state.apply(tx)
}

fn handle_mouse(state: &EditorState, view: &mut ViewState, ev: &MouseEvent) -> Action {
    handle_mouse_with_mods(state, view, ev, Modifiers::default())
}

pub fn handle_mouse_with_mods(
    state: &EditorState,
    view: &mut ViewState,
    ev: &MouseEvent,
    mods: Modifiers,
) -> Action {
    match ev {
        MouseEvent::Down { button: MouseButton::Left, x, y, click_count } => {
            (Cmd { state, view }).mouse_down(*x, *y, *click_count, mods)
        }
        MouseEvent::Drag { x, y, button: MouseButton::Left } => {
            (Cmd { state, view }).mouse_drag(*x, *y)
        }
        MouseEvent::Up { button: MouseButton::Left, x, y } => {
            (Cmd { state, view }).mouse_up(*x, *y)
        }
        _ => Action::None,
    }
}

impl<'a> Cmd<'a> {

fn mouse_down(
    &mut self,
    x: f32,
    y: f32,
    click_count: u8,
    mods: Modifiers,
) -> Action {
    let state = self.state;
    let view = &mut *self.view;
    view.touch();
    // Check for a clickable decoration first.
    if let Some(zone) = view.click_zones.iter().find(|z| z.rect.contains(x, y)) {
        return Action::Click(zone.action.clone());
    }
    let pos = view_to_buffer(state, view, x, y);

    // Plain click (no modifiers, no multi-click) inside an existing
    // non-empty selection arms a possible text drag — we don't move
    // the caret yet, we wait to see whether the user drags or releases.
    if click_count == 1
        && !mods.shift
        && !mods.alt
        && !mods.primary()
        && pos_in_any_nonempty_range(state, pos)
    {
        // 10px threshold matches CodeMirror 6's drag-to-move default. With
        // a smaller threshold, micro-jitter inside an existing selection
        // would mis-trigger a text drag rather than letting the user
        // click-collapse the selection and begin a new one.
        view.drag = DragState::MaybeDraggingSelection { start: (x, y), threshold: 10.0 };
        return Action::None;
    }

    // Alt-only Down (and not inside an existing selection) starts a
    // rectangular/column selection — place a single caret at `pos` and
    // arm `RectangleSelecting`. Alt+Shift retains the existing
    // multicursor-add semantics below.
    if click_count == 1
        && mods.alt
        && !mods.shift
        && !mods.primary()
        && !pos_in_any_nonempty_range(state, pos)
    {
        view.drag = DragState::RectangleSelecting { start_xy: (x, y) };
        return Action::state_only(apply_selection(state, Selection::single(pos)));
    }

    let sel = match click_count {
        2 => {
            // Single code path: run `view.double_click_re` against the
            // clicked line's content. The default (`\w+`) reproduces the
            // historic Unicode-word behavior; users override via
            // `editor.double_click_pattern`. No match → plain caret.
            let line = state.doc.byte_to_line(pos);
            let line_start = state.doc.line_to_byte(line);
            let text = state.doc.line_str(line);
            let local = pos - line_start;
            match pattern_span_at(&text, local, &view.double_click_re) {
                Some((s, e)) => {
                    Selection::from_range(SelRange::new(line_start + s, line_start + e))
                }
                None => Selection::single(pos),
            }
        }
        3 => {
            // Single code path: run `view.triple_click_re` against the
            // clicked line **including** its trailing newline (the slice
            // `line_start..line_end_with_newline`), so the default `.*\n?`
            // reproduces the previous whole-line-incl-newline behavior.
            let line = state.doc.byte_to_line(pos);
            let line_start = state.doc.line_to_byte(line);
            let line_end = if line + 1 < state.doc.len_lines() {
                state.doc.line_to_byte(line + 1)
            } else {
                state.doc.len_bytes()
            };
            let text = state.doc.slice(line_start..line_end).to_string();
            let local = pos - line_start;
            match pattern_span_at(&text, local, &view.triple_click_re) {
                Some((s, e)) => {
                    Selection::from_range(SelRange::new(line_start + s, line_start + e))
                }
                None => Selection::single(pos),
            }
        }
        _ if mods.alt || (mods.primary() && !mods.shift) => {
            multicursor::add_cursor(state, pos)
        }
        _ if mods.shift => {
            let anchor = state.selection.main().anchor.offset();
            Selection::from_range(SelRange::new(anchor, pos))
        }
        _ => Selection::single(pos),
    };
    // Arm a drag from the selection we just made: a plain click anchors a
    // point (lo == hi), a double/triple click anchors the whole word/line so a
    // subsequent drag (or a no-motion stray Drag — see `mouse_drag`) extends
    // rather than collapses it.
    let main = sel.main();
    view.drag = DragState::MaybeSelecting { lo: main.start(), hi: main.end() };
    Action::state_only(apply_selection(state, sel))
}

fn mouse_drag(&mut self, x: f32, y: f32) -> Action {
    let state = self.state;
    let view = &mut *self.view;
    match view.drag {
        DragState::MaybeSelecting { lo, hi } => {
            view.touch();
            let head = view_to_buffer(state, view, x, y);
            // Union the anchored range with the pointer: while the pointer
            // stays within [lo, hi] the selection is exactly that range (so a
            // double-click word survives jitter and the no-motion stray Drag
            // the translate layer emits each held frame). Dragging past either
            // edge extends from the far edge, with the head on the moving side.
            let start = lo.min(head);
            let end = hi.max(head);
            let range = if head < lo {
                SelRange::new(end, start)
            } else {
                SelRange::new(start, end)
            };
            Action::state_only(apply_selection(state, Selection::from_range(range)))
        }
        DragState::MaybeDraggingSelection { start, threshold } => {
            let dx = x - start.0;
            let dy = y - start.1;
            if (dx * dx + dy * dy).sqrt() > threshold {
                let drop_caret = view_to_buffer(state, view, x, y);
                view.drag = DragState::DraggingSelection { drop_caret };
                view.touch();
            }
            Action::None
        }
        DragState::DraggingSelection { .. } => {
            let drop_caret = view_to_buffer(state, view, x, y);
            view.drag = DragState::DraggingSelection { drop_caret };
            view.touch();
            Action::None
        }
        DragState::RectangleSelecting { start_xy } => {
            view.touch();
            // Build a multi-range Selection covering one SelRange per buffer
            // line intersecting the vertical span `[start_xy.1, y]`, each
            // spanning from x→byte(min_x) to x→byte(max_x) on its own line.
            // The main range is the line the pointer is currently on.
            let (sx, sy) = start_xy;
            let (cx, cy) = (x, y);
            let y_lo = sy.min(cy);
            let y_hi = sy.max(cy);
            let x_lo = sx.min(cx);
            let x_hi = sx.max(cx);
            let line_lo = view
                .height_map
                .line_at_y(y_lo + view.scroll_y)
                .min(state.doc.len_lines().saturating_sub(1));
            let line_hi = view
                .height_map
                .line_at_y(y_hi + view.scroll_y)
                .min(state.doc.len_lines().saturating_sub(1));
            let mut ranges: Vec<SelRange> = Vec::with_capacity(line_hi - line_lo + 1);
            for line in line_lo..=line_hi {
                let a = view_to_buffer_at_line(state, view, x_lo, line);
                let b = view_to_buffer_at_line(state, view, x_hi, line);
                ranges.push(SelRange::new(a, b));
            }
            let cur_line = view
                .height_map
                .line_at_y(cy + view.scroll_y)
                .min(state.doc.len_lines().saturating_sub(1));
            let main = cur_line.saturating_sub(line_lo).min(ranges.len() - 1);
            let sel = Selection::from_ranges(ranges, main);
            Action::state_only(apply_selection(state, sel))
        }
        DragState::Idle => Action::None,
    }
}

}

/// Map a widget-local `x` to a byte offset on buffer `line`. Mirrors
/// `view_to_buffer`'s x→column approximation, including the live-preview
/// adjustments for header lines (hidden leading markers + scaled glyphs);
/// takes the line explicitly so callers building rectangle selections can
/// iterate rows without recomputing y mapping.
pub fn view_to_buffer_at_line(
    state: &EditorState,
    view: &ViewState,
    x: f32,
    line: usize,
) -> usize {
    let line = line.min(state.doc.len_lines().saturating_sub(1));
    let line_start = state.doc.line_to_byte(line);
    let line_text = state.doc.line_str(line);
    // Strip any trailing newline so the column never lands past EOL.
    let text_no_nl = line_text.trim_end_matches('\n');
    let col_x = (x - view.content_origin_x()).max(0.0);
    line_start + col_x_to_line_byte(view, line_start, text_no_nl, true, col_x)
}

/// Effective glyph width used for x→column mapping, accounting for the
/// per-line live-preview font scale (headings render at `font_scale`× the
/// base monospace cell). Mirrors the renderer's per-line scale probe in
/// `prewrap_visible`: the max `Mark.font_scale` covering the line.
fn line_font_scale(view: &ViewState, line_start: usize, line_text: &str) -> f32 {
    let probe = line_start..(line_start + line_text.len()).max(line_start + 1);
    let mut scale = 1.0_f32;
    for layer in &view.decorations.layers {
        for (_r, deco) in layer.iter_overlapping(probe.clone()) {
            if let Decoration::Mark(ms) = deco
                && let Some(s) = ms.font_scale
                && s > scale
            {
                scale = s;
            }
        }
    }
    scale
}

/// Byte length of the run of hidden `Replace` markers anchored at the start of
/// the line (e.g. a heading's `## ` prefix, which renders zero-width in live
/// preview). Used to skip those source bytes when mapping a click to a byte:
/// the click lands on the first *visible* glyph, which is the content after
/// the marker, not the marker itself. Only contiguous hidden replacements
/// starting exactly at `line_start` count; mid-line replacements (inline-code
/// backticks, emphasis stars) are left to the verbatim column walk since the
/// glyphs around them still occupy their source columns closely enough.
fn leading_hidden_bytes(view: &ViewState, line_start: usize, line_len: usize) -> usize {
    let mut covered = 0usize;
    // Re-scan from the growing frontier so multiple stacked hidden replacements
    // (rare, but possible) chain into one contiguous skipped prefix.
    loop {
        let frontier = line_start + covered;
        let mut grew = false;
        for layer in &view.decorations.layers {
            for (r, deco) in layer.iter_overlapping(frontier..frontier + 1) {
                let hidden = matches!(
                    deco,
                    Decoration::Replace { display } if display.as_ref().is_none_or(smol_str::SmolStr::is_empty)
                );
                if !(hidden && r.start <= frontier && r.end > frontier) {
                    continue;
                }
                let end_local = (r.end - line_start).min(line_len);
                if end_local > covered {
                    covered = end_local;
                    grew = true;
                }
            }
        }
        if !grew {
            return covered;
        }
    }
}

/// Map a text-area `col_x` to a line-local byte offset for one (visual) line
/// of source `text`. Folds in the live-preview header adjustments: glyphs are
/// scaled by the line's `font_scale`, and a hidden leading marker run is
/// skipped so the first visible glyph maps to the first content byte rather
/// than the marker. `at_line_start` is false for soft-wrap continuation rows,
/// where there is no leading marker to skip.
fn col_x_to_line_byte(
    view: &ViewState,
    line_start: usize,
    text: &str,
    at_line_start: bool,
    col_x: f32,
) -> usize {
    let measured = view.wrap_map.char_width();
    let base_char_w = if measured > 0.5 { measured } else { view.font_size * 0.6 };
    let scale = line_font_scale(view, line_start, text);
    let eff_char_w = (base_char_w * scale).max(0.5);
    let raw_hidden = if at_line_start {
        leading_hidden_bytes(view, line_start, text.len()).min(text.len())
    } else {
        0
    };
    // Snap the hidden length down to a char boundary defensively.
    let mut hidden = raw_hidden;
    while hidden > 0 && !text.is_char_boundary(hidden) {
        hidden -= 1;
    }
    let visible = &text[hidden..];
    let col = ((col_x / eff_char_w).round() as usize).min(visible.chars().count());
    let mut byte = visible.len();
    for (i, (b, _)) in visible.char_indices().enumerate() {
        if i == col {
            byte = b;
            break;
        }
    }
    hidden + byte
}

impl<'a> Cmd<'a> {

fn mouse_up(&mut self, x: f32, y: f32) -> Action {
    let state = self.state;
    let view = &mut *self.view;
    let prev = view.drag;
    view.drag = DragState::Idle;
    match prev {
        DragState::DraggingSelection { drop_caret } => {
            // Apply a text drag: remove the main selection range and
            // reinsert it at `drop_caret`. If the drop falls inside the
            // original range, cancel.
            let src = state.selection.main().range();
            if drop_caret >= src.start && drop_caret <= src.end {
                view.touch();
                return Action::None;
            }
            let text = state.doc.slice(src.clone()).to_string();
            let len = text.len();
            let mut edits: Vec<(std::ops::Range<usize>, String)> = if drop_caret > src.end {
                vec![(drop_caret..drop_caret, text), (src.clone(), String::new())]
            } else {
                vec![(src.clone(), String::new()), (drop_caret..drop_caret, text)]
            };
            edits.sort_by_key(|(r, _)| r.start);
            let changes = ChangeSet::of(state.doc.len_bytes(), edits);
            let new_start = if drop_caret > src.end {
                drop_caret - (src.end - src.start)
            } else {
                drop_caret
            };
            let new_sel = Selection::from_range(SelRange::new(new_start, new_start + len));
            let tx = Transaction::new(changes)
                .with_edit_type(EditType::Other)
                .with_selection(new_sel);
            view.touch();
            Action::doc(state.apply(tx.clone()), tx)
        }
        DragState::MaybeDraggingSelection { .. } => {
            // No drag occurred — treat as a plain click: collapse the
            // selection to a single caret at the clicked position.
            let pos = view_to_buffer(state, view, x, y);
            view.touch();
            Action::state_only(apply_selection(state, Selection::single(pos)))
        }
        _ => Action::None,
    }
}

}

fn pos_in_any_nonempty_range(state: &EditorState, pos: usize) -> bool {
    state.selection.ranges().iter().any(|r| !r.is_empty() && pos >= r.start() && pos < r.end())
}

/// Byte span (relative to `text`) of the first regex match that contains the
/// click column `local`. `None` when no match covers it, so the caller falls
/// back to its built-in behavior. End-inclusive (`local <= end`) so a click at
/// a match's trailing edge still selects it, mirroring the Unicode-word path.
/// Used by double/triple-click when `editor.{double,triple}_click_pattern` is
/// set. status: click-select-pattern
fn pattern_span_at(text: &str, local: usize, re: &regex::Regex) -> Option<(usize, usize)> {
    re.find_iter(text).map(|m| (m.start(), m.end())).find(|&(s, e)| local >= s && local <= e)
}

fn scroll_by(view: &mut ViewState, delta_y: f32) {
    view.scroll_y = (view.scroll_y - delta_y).max(0.0);
    clamp_scroll(view);
}

/// Clamp `scroll_y` to `[0, total - height + scroll_past_end*height]`. Shared
/// by wheel scroll and caret-follow so both honor the same bounds, including
/// the `scroll_past_end` overshoot allowance.
fn clamp_scroll(view: &mut ViewState) {
    view.scroll_y = view.scroll_y.max(0.0);
    let max = (view.height_map.total_height() - view.height
        + view.scroll_past_end * view.height)
        .max(0.0);
    if view.scroll_y > max {
        view.scroll_y = max;
    }
}

/// Scroll the minimum amount so the caret's line is within the visible band
/// `[scroll_y, scroll_y + height]`. No-op when the caret is already visible.
/// Called (deferred) after edits so typing follows the caret; consumed by the
/// egui widget once the height map reflects the post-edit doc. Granularity is
/// the buffer line — with wrap on, a caret deep inside a long wrapped line
/// scrolls that line's top into view rather than the exact visual row.
pub fn scroll_caret_into_view(state: &EditorState, view: &mut ViewState) {
    let lines = state.doc.len_lines();
    if lines == 0 || view.height <= 0.0 {
        return;
    }
    let caret = state.selection.main().head.offset().min(state.doc.len_bytes());
    let line = state.doc.byte_to_line(caret).min(lines - 1);
    let top = view.height_map.y_at_text(line);
    let bottom = top + view.height_map.text_height(line).max(view.line_height);

    if top < view.scroll_y {
        view.scroll_y = top;
    } else if bottom > view.scroll_y + view.height {
        view.scroll_y = bottom - view.height;
    } else {
        return;
    }
    clamp_scroll(view);
}

/// Map widget-local (x, y) to a byte offset in the doc. `x` is widget-local
/// (including gutter); the mapper subtracts the view's current content origin
/// ([`ViewState::content_origin_x`]) — the gutter width with line numbers on,
/// or a small pad when the gutter is hidden — so the column is measured from
/// where the text actually starts regardless of the show-line-numbers toggle.
pub fn view_to_buffer(state: &EditorState, view: &ViewState, x: f32, y: f32) -> usize {
    let line_y = y + view.scroll_y;
    let line = view.height_map.line_at_y(line_y).min(state.doc.len_lines() - 1);
    let line_start = state.doc.line_to_byte(line);
    let line_text = state.doc.line_str(line);

    // With wrap on, figure out which vline within this buffer line the y falls
    // into, then offset the local text slice to that vline.
    let (vline_start_byte, vline_end_byte) = if view.wrap_map.enabled() {
        if let Some(w) = view.wrap_map.peek(line) {
            let buf_line_top = view.height_map.y_at_text(line);
            let local_y = (line_y - buf_line_top).max(0.0);
            let vline_idx =
                ((local_y / view.line_height).floor() as usize).min(w.visual_count() - 1);
            let (s, e) = w.vline_range(vline_idx);
            (s, e)
        } else {
            (0, line_text.len())
        }
    } else {
        (0, line_text.len())
    };
    let vline_text = &line_text[vline_start_byte..vline_end_byte];

    let col_x = (x - view.content_origin_x()).max(0.0);
    // Map x→byte through the shared mapper. It uses the measured monospace
    // "M" width the renderer cached on `wrap_map` from a real font layout
    // (the previous `font_size * 0.55` heuristic mispredicted column
    // positions, worsening on long lines), and additionally folds in the
    // live-preview header adjustments: glyphs scaled by the line's
    // `font_scale` and a hidden leading marker run (`## `) skipped so the
    // click lands on the first visible glyph. Only the first visual row of a
    // buffer line carries the leading marker.
    let at_line_start = vline_start_byte == 0;
    let local = col_x_to_line_byte(view, line_start, vline_text, at_line_start, col_x);
    line_start + vline_start_byte + local
}

/// Helper exposed for backends that need to construct text-insertion actions
/// directly (e.g. on receipt of platform `Text` events that arrive separately
/// from key events).
pub fn insert_smol(state: &EditorState, view: &mut ViewState, s: &SmolStr) -> Action {
    insert_text(state, view, s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_core::decoration::{Decoration, MarkStyle};
    use editor_core::rangeset::RangeSet;

    /// Build a view whose decorations mirror what the markdown live-preview
    /// provider emits for an ATX header line: a hidden `Replace` over the
    /// `## ` prefix and a scaled `Mark` over the whole heading. `char_width`
    /// is seeded so the x→byte mapper has a real glyph width to divide by.
    fn header_view(prefix_len: usize, heading_range: std::ops::Range<usize>, scale: f32) -> ViewState {
        let mut view = ViewState::default();
        view.wrap_map.set_char_width(10.0);
        let entries = vec![
            (
                heading_range.start..heading_range.start + prefix_len,
                Decoration::Replace { display: None },
            ),
            (
                heading_range,
                Decoration::Mark(MarkStyle { font_scale: Some(scale), bold: true, ..MarkStyle::default() }),
            ),
        ];
        view.decorations.push(RangeSet::from_iter(entries));
        view
    }

    /// A click on a header line must land on the first *content* glyph, not on
    /// the hidden `## ` marker, and must use the scaled glyph width. Before the
    /// fix the mapper counted columns into the source text (including the
    /// marker) at the base width, so clicks landed `prefix_len` bytes early and
    /// drifted further the wider the click x.
    #[test]
    fn header_click_skips_hidden_marker_and_scales() {
        let doc = "## Heading\n";
        let state = EditorState::new(doc);
        // H2 scale; prefix "## " is 3 bytes; "Heading" starts at byte 3.
        let view = header_view(3, 0..10, 1.6);
        let gutter = view.gutter_width;
        let eff_w = 10.0 * 1.6; // base char width * font scale

        // Click at the left edge of the content -> the 'H' at byte 3.
        assert_eq!(view_to_buffer_at_line(&state, &view, gutter + 0.0, 0), 3);
        // Click roughly over the 3rd visible glyph -> byte 3 + 2 = 5 ('a').
        let x = gutter + eff_w * 2.0;
        assert_eq!(view_to_buffer_at_line(&state, &view, x, 0), 5);
        // Click well past the end clamps to end of "Heading" (byte 10).
        let x_far = gutter + eff_w * 50.0;
        assert_eq!(view_to_buffer_at_line(&state, &view, x_far, 0), 10);
    }

    /// Control: with no decorations the mapper is the plain monospace column
    /// walk — same click x lands at a different (un-skipped, un-scaled) byte.
    #[test]
    fn plain_line_click_uses_base_width_no_skip() {
        let doc = "Heading\n";
        let state = EditorState::new(doc);
        let mut view = ViewState::default();
        view.wrap_map.set_char_width(10.0);
        let gutter = view.gutter_width;
        // Click ~2 base-width glyphs in -> byte 2 ('a'), no +3 marker skip.
        assert_eq!(view_to_buffer_at_line(&state, &view, gutter + 20.0, 0), 2);
    }

    /// A bare DecorationSet with no `font_scale`/`Replace` must leave the
    /// mapping identical to the undecorated case (scale defaults to 1.0,
    /// hidden prefix is 0). Guards against the scale/skip helpers firing on
    /// non-header lines that merely carry inline marks.
    #[test]
    fn inline_mark_without_scale_does_not_shift() {
        let doc = "abcdefgh\n";
        let state = EditorState::new(doc);
        let mut view = ViewState::default();
        view.wrap_map.set_char_width(10.0);
        let entries = vec![(
            2..5,
            Decoration::Mark(MarkStyle { italic: true, ..MarkStyle::default() }),
        )];
        view.decorations.push(RangeSet::from_iter(entries));
        let gutter = view.gutter_width;
        assert_eq!(view_to_buffer_at_line(&state, &view, gutter + 30.0, 0), 3);
    }

    /// The content origin (what the click→byte mapper subtracts and the painter
    /// adds) tracks the show-line-numbers toggle: full gutter width when shown,
    /// the small pad when hidden. Regression for `bug-gutter-toggle-mouse-offset`,
    /// where the inverse mapper unconditionally subtracted `gutter_width` so
    /// clicks with the gutter off landed `gutter_width - pad` px too far left.
    #[test]
    fn content_origin_tracks_gutter_toggle() {
        let mut view = ViewState::default();
        assert!(!view.hide_gutter);
        assert_eq!(view.content_origin_x(), view.gutter_width);
        view.hide_gutter = true;
        assert_eq!(view.content_origin_x(), crate::viewport::HIDDEN_GUTTER_PAD);
        // Sanity: hidden pad is far smaller than the normal gutter, which is
        // exactly the offset the old code mis-applied.
        assert!(view.content_origin_x() < view.gutter_width);
    }

    /// With the gutter hidden, a click at a given widget-local x must map to the
    /// SAME byte the painter would place under the pointer — i.e. the column is
    /// measured from the hidden-gutter pad, not from the full gutter width.
    #[test]
    fn click_maps_consistently_with_gutter_hidden() {
        let doc = "abcdefgh\n";
        let state = EditorState::new(doc);
        let mut view = ViewState::default();
        view.wrap_map.set_char_width(10.0);
        view.hide_gutter = true;
        // Painter draws the first glyph at `content_origin_x()`. A click two
        // glyph-widths past it lands on byte 2 ('c').
        let x = view.content_origin_x() + 20.0;
        assert_eq!(view_to_buffer_at_line(&state, &view, x, 0), 2);
        // The same widget-local x with the gutter SHOWN (origin = gutter_width)
        // would be left of the text and clamp to byte 0 — proving the mapper now
        // honors the toggle rather than assuming a fixed gutter width.
        view.hide_gutter = false;
        assert_eq!(view_to_buffer_at_line(&state, &view, x, 0), 0);
    }
}
