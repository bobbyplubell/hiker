//! Default command set. Translates [`InputEvent`]s into [`Transaction`]s and
//! direct selection / scroll mutations on the [`ViewState`].

use editor_core::{ChangeSet, EditType, EditorState, SelRange, Selection, Transaction};
use smol_str::SmolStr;

use crate::completion::{CompletionItem, CompletionKind};
use crate::snippet::{self, Snippet};
use crate::event::{
    ImeEvent, InputEvent, Key, KeyEvent, Modifiers, MouseButton, MouseEvent, NamedKey,
};
use crate::motion::{self, Direction};
use crate::multicursor;
use crate::view::{ClickAction, DragState, ViewState};

/// Outcome of handling one input event.
pub enum Action {
    /// Replace the editor state with the given new state.
    Replace(EditorState),
    /// Just touch the view (scroll changed, drag updated, etc.).
    None,
    /// Request a clipboard write.
    Copy(String),
    /// Click landed on a clickable decoration zone (e.g. an Expander).
    Click(ClickAction),
}

pub fn handle(state: &EditorState, view: &mut ViewState, event: &InputEvent) -> Action {
    handle_inner(state, view, event)
}

/// Apply a transaction, then — if a snippet is active — map the snippet's
/// anchors through the change and run a mirror sync so the primary cursor's
/// text is propagated to every mirror span. Returns the final state.
fn apply_with_snippet(state: &EditorState, view: &mut ViewState, tx: Transaction) -> EditorState {
    let changes = tx.changes.clone();
    let after = state.apply(tx);
    if view.snippet.is_active() {
        snippet::map_through(&mut view.snippet, &changes);
        if let Some(sync_tx) = snippet::mirror_sync(&after, &view.snippet) {
            let sync_changes = sync_tx.changes.clone();
            let synced = after.apply(sync_tx);
            snippet::map_through(&mut view.snippet, &sync_changes);
            return synced;
        }
    }
    after
}

fn handle_inner(state: &EditorState, view: &mut ViewState, event: &InputEvent) -> Action {
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
        InputEvent::Ime(ev) => handle_ime(state, view, ev),
        InputEvent::Mouse(ev) => handle_mouse(state, view, ev),
        InputEvent::Scroll { delta_y, .. } => {
            scroll_by(view, *delta_y);
            Action::None
        }
        InputEvent::Focus(_) => Action::None,
        InputEvent::Paste(s) => insert_text(state, view, s),
        InputEvent::Copy => copy_selection(state),
        InputEvent::Cut => {
            let copy = copy_selection(state);
            if let Action::Copy(text) = copy {
                let tx = state.delete_at_selections();
                let new_state = state.apply(tx);
                view.touch();
                // Return the copy then apply state by chaining via a small trick:
                // we can only return one Action, so prefer the state replace and
                // ship the copy via a side return.
                // Hack for v1: handle cut via two steps in the widget instead.
                view.touch();
                let _ = text;
                return Action::Replace(new_state);
            }
            Action::None
        }
    }
}

/// Motion-only key dispatch. Extracted from `handle_key` to keep the
/// dispatcher's cognitive complexity under the clippy budget.
/// Returns `Some(action)` if the key was a motion key (arrows / page /
/// home / end / column-cursor add), `None` otherwise.
fn handle_motion_key(
    state: &EditorState,
    view: &mut ViewState,
    key: Key,
    mods: Modifiers,
    extend: bool,
    word_jump: bool,
) -> Option<Action> {
    use Direction::*;
    let action = match key {
        Key::Named(NamedKey::ArrowUp) if mods.alt && mods.primary() => {
            let sel = multicursor::add_vertical_cursor(state, false);
            Action::Replace(apply_selection(state, sel))
        }
        Key::Named(NamedKey::ArrowDown) if mods.alt && mods.primary() => {
            let sel = multicursor::add_vertical_cursor(state, true);
            Action::Replace(apply_selection(state, sel))
        }
        Key::Named(NamedKey::ArrowLeft) => {
            let layers = view.decorations.layers.as_slice();
            let sel = if word_jump {
                motion::move_word(state, Left, extend, layers)
            } else {
                motion::move_char(state, Left, extend, layers)
            };
            Action::Replace(apply_selection(state, sel))
        }
        Key::Named(NamedKey::ArrowRight) => {
            let layers = view.decorations.layers.as_slice();
            let sel = if word_jump {
                motion::move_word(state, Right, extend, layers)
            } else {
                motion::move_char(state, Right, extend, layers)
            };
            Action::Replace(apply_selection(state, sel))
        }
        Key::Named(NamedKey::ArrowUp) => {
            let wrap = if view.wrap_map.enabled() { Some(&view.wrap_map) } else { None };
            let sel = motion::move_vertical_wrapped(state, Up, extend, 1, wrap);
            Action::Replace(apply_selection(state, sel))
        }
        Key::Named(NamedKey::ArrowDown) => {
            let wrap = if view.wrap_map.enabled() { Some(&view.wrap_map) } else { None };
            let sel = motion::move_vertical_wrapped(state, Down, extend, 1, wrap);
            Action::Replace(apply_selection(state, sel))
        }
        Key::Named(NamedKey::PageUp) => {
            let lines = ((view.height / view.line_height).floor() as usize).max(1);
            let wrap = if view.wrap_map.enabled() { Some(&view.wrap_map) } else { None };
            let sel = motion::move_vertical_wrapped(state, Up, extend, lines, wrap);
            Action::Replace(apply_selection(state, sel))
        }
        Key::Named(NamedKey::PageDown) => {
            let lines = ((view.height / view.line_height).floor() as usize).max(1);
            let wrap = if view.wrap_map.enabled() { Some(&view.wrap_map) } else { None };
            let sel = motion::move_vertical_wrapped(state, Down, extend, lines, wrap);
            Action::Replace(apply_selection(state, sel))
        }
        Key::Named(NamedKey::Home) => {
            let sel = if mods.primary() {
                motion::move_doc_edge(state, false, extend)
            } else {
                motion::move_line_edge(state, false, extend)
            };
            Action::Replace(apply_selection(state, sel))
        }
        Key::Named(NamedKey::End) => {
            let sel = if mods.primary() {
                motion::move_doc_edge(state, true, extend)
            } else {
                motion::move_line_edge(state, true, extend)
            };
            Action::Replace(apply_selection(state, sel))
        }
        _ => return None,
    };
    Some(action)
}

fn handle_key(state: &EditorState, view: &mut ViewState, key: Key, mods: Modifiers) -> Action {
    view.touch();
    view.ime.clear_preedit();
    // Any key event invalidates a pending auto-pair skip — the user moved on.
    view.autopair_skip_at = None;

    // Search panel keybindings. Cmd-F / Ctrl-F always opens. While the panel
    // is active, Enter / Shift-Enter / Escape are intercepted for match
    // navigation and dismissal BEFORE any other handler.
    if let Some(action) = handle_search_key(view, key, mods) {
        return action;
    }

    // Snippet cycling: intercept Tab / Shift-Tab / Escape while a snippet
    // expansion is active. Must run BEFORE the existing Tab indent path
    // and before completion handling so the user's Tab advances the stop.
    if view.snippet.is_active() {
        if let Some(action) = handle_snippet_key(state, view, key, mods) {
            return action;
        }
    }

    if view.completion.active {
        if let Some(action) = handle_completion_key(state, view, key, mods) {
            return action;
        }
    }

    let extend = mods.shift;

    // Word-granularity if alt (mac) or ctrl (non-mac). egui maps OS primary to `meta`
    // on mac and `ctrl` on Linux/Windows; word-jump is alt on mac, ctrl on linux/win.
    // We use `alt` here for word boundaries — most platforms accept it.
    let word_jump = mods.alt;

    if let Some(action) = handle_motion_key(state, view, key, mods, extend, word_jump) {
        return action;
    }

    match key {
        Key::Named(NamedKey::Backspace) => {
            let tx = state.delete_at_selections();
            Action::Replace(apply_with_snippet(state, view, tx))
        }
        Key::Named(NamedKey::Delete) => {
            let tx = delete_forward(state);
            Action::Replace(apply_with_snippet(state, view, tx))
        }
        Key::Named(NamedKey::Enter) if mods.is_empty() => {
            if let Some(provider) = view.indent_provider.clone() {
                if let Some(tx) = provider.on_enter(state) {
                    return Action::Replace(state.apply(tx));
                }
            }
            insert_text(state, view, "\n")
        }
        Key::Named(NamedKey::Enter) if mods.shift => insert_text(state, view, "\n"),
        Key::Named(NamedKey::Tab) if mods.is_empty() => indent_tab(state, view),
        Key::Named(NamedKey::Tab) if mods.shift && !mods.primary() && !mods.alt => {
            shift_tab_outdent(state, view)
        }
        // Note: don't handle plain Space here. egui emits BOTH a Key
        // event and a Text(" ") event for one physical space press; the
        // Text branch inserts the space, so handling Space here would
        // double-insert. Modifier-bearing Space chords (Ctrl-Space etc.)
        // are intercepted higher up in `app::keybinds`.
        Key::Char('a') | Key::Char('A') if mods.primary_only() => {
            let sel = motion::select_all(state);
            Action::Replace(apply_selection(state, sel))
        }
        Key::Char('z') | Key::Char('Z') if mods.primary_only() => {
            if let Some(next) = state.undo() {
                Action::Replace(next)
            } else {
                Action::None
            }
        }
        Key::Char('z') | Key::Char('Z') if mods.primary() && mods.shift && !mods.alt => {
            if let Some(next) = state.redo() {
                Action::Replace(next)
            } else {
                Action::None
            }
        }
        Key::Char('y') | Key::Char('Y') if mods.primary_only() => {
            if let Some(next) = state.redo() {
                Action::Replace(next)
            } else {
                Action::None
            }
        }
        Key::Char('c') | Key::Char('C') if mods.primary_only() => copy_selection(state),
        // Cmd-D / Ctrl-D — add next occurrence of selection.
        Key::Char('d') | Key::Char('D') if mods.primary_only() => {
            let sel = multicursor::add_next_occurrence(state);
            Action::Replace(apply_selection(state, sel))
        }
        // Escape collapses to the main cursor.
        Key::Named(NamedKey::Escape) => {
            let main = state.selection.main().head.offset();
            let sel = editor_core::Selection::single(main);
            Action::Replace(apply_selection(state, sel))
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
        && state.selection.ranges().iter().all(|r| r.is_empty())
    {
        if let Some(tx) = crate::autopair::autopair_skip(state, saved_skip, s) {
            return Action::Replace(apply_with_snippet(state, view, tx));
        }
    }

    // Auto-pair: only when typing a single char and no selection text.
    if s.chars().count() == 1
        && state.selection.ranges().iter().all(|r| r.is_empty())
    {
        if let Some(tx) = crate::autopair::autopair_transform(state, s) {
            let new_state = apply_with_snippet(state, view, tx);
            // Record the skip marker: cursor is between open and close, so the
            // close char ends one char-len past the cursor.
            if let Some(first) = s.chars().next() {
                if let Some(pair) = crate::autopair::DEFAULT_PAIRS
                    .iter()
                    .find(|p| p.open == first)
                {
                    let cursor = new_state.selection.main().head.offset();
                    view.autopair_skip_at = Some(cursor + pair.close.len_utf8());
                }
            }
            maybe_open_completion(&new_state, view, s);
            return Action::Replace(new_state);
        }
    }
    let tx = state.insert_at_selections(s);
    let new_state = apply_with_snippet(state, view, tx);
    if s.chars().count() == 1 {
        maybe_open_completion(&new_state, view, s);
    } else if view.completion.active {
        // Multi-char paste closes the popup.
        view.completion.close();
    }
    Action::Replace(new_state)
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
    if !triggered {
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

/// Search-panel key interception. Cmd-F / Ctrl-F opens the panel. When the
/// panel is active, Enter advances to the next match, Shift-Enter to the
/// previous, and Escape closes. Returns `Some(Action::None)` when the key was
/// consumed; `None` to fall through to other handlers.
fn handle_search_key(view: &mut ViewState, key: Key, mods: Modifiers) -> Option<Action> {
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
    state: &EditorState,
    view: &mut ViewState,
    key: Key,
    mods: Modifiers,
) -> Option<Action> {
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
            Some(commit_completion(state, view))
        }
        Key::Named(NamedKey::Backspace) => {
            if view.completion.query.is_empty() {
                view.completion.close();
                None
            } else {
                view.completion.query.pop();
                // Apply the backspace to the doc, then refilter.
                let tx = state.delete_at_selections();
                let new_state = state.apply(tx);
                let pos = new_state.selection.main().head.offset();
                let items = gather_matches(&new_state, view, pos);
                if items.is_empty() {
                    view.completion.close();
                } else {
                    view.completion.items = items;
                    view.completion.selected = 0;
                }
                Some(Action::Replace(new_state))
            }
        }
        _ => None,
    }
}

fn commit_completion(state: &EditorState, view: &mut ViewState) -> Action {
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
    Action::Replace(state.apply(tx))
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
    let changes = tx.changes.clone();
    let after = state.apply(tx);
    // Anchors were built against positions in the new doc, so no mapping needed.
    let _ = changes;
    let sel = snippet::selection_for_stop(&snip_state, 0)
        .unwrap_or_else(|| Selection::single(pos + snip.text().len()));
    let with_sel = Transaction::new(ChangeSet::empty(after.doc.len_bytes())).with_selection(sel);
    let after = after.apply(with_sel);
    // If there is only `$0` (or no stops at all), there is nothing to cycle.
    if snip_state.stops.is_empty() {
        snip_state.cancel();
    }
    view.snippet = snip_state;
    Action::Replace(after)
}

/// Snippet key handling. Returns `Some(action)` if the key was consumed.
fn handle_snippet_key(
    state: &EditorState,
    view: &mut ViewState,
    key: Key,
    mods: Modifiers,
) -> Option<Action> {
    match key {
        Key::Named(NamedKey::Escape) if mods.is_empty() => {
            view.snippet.cancel();
            Some(Action::None)
        }
        Key::Named(NamedKey::Tab) if mods.is_empty() => {
            Some(advance_snippet(state, view, 1))
        }
        Key::Named(NamedKey::Tab) if mods.shift && !mods.primary() && !mods.alt => {
            Some(advance_snippet(state, view, -1))
        }
        _ => None,
    }
}

fn advance_snippet(state: &EditorState, view: &mut ViewState, delta: i32) -> Action {
    // First, sync any mirrors at the *current* stop into the doc before moving.
    let mut working = state.clone();
    if let Some(tx) = snippet::mirror_sync(&working, &view.snippet) {
        let changes = tx.changes.clone();
        working = working.apply(tx);
        snippet::map_through(&mut view.snippet, &changes);
    }
    let n = view.snippet.stops.len() as i32;
    if n == 0 {
        view.snippet.cancel();
        return Action::Replace(working);
    }
    let next = view.snippet.current as i32 + delta;
    if next < 0 || next >= n {
        // Past the final stop — cancel and leave caret where the doc has it.
        view.snippet.cancel();
        return Action::Replace(working);
    }
    view.snippet.current = next as usize;
    let sel = match snippet::selection_for_stop(&view.snippet, view.snippet.current) {
        Some(s) => s,
        None => {
            view.snippet.cancel();
            return Action::Replace(working);
        }
    };
    let tx = Transaction::new(ChangeSet::empty(working.doc.len_bytes())).with_selection(sel);
    Action::Replace(working.apply(tx))
}

/// Tab: insert 4 spaces at every caret. SPEC §9.14 leaves the smarter
/// "indent the entire selected block" for a future revision; the v1 rule is
/// "insert 4 spaces at the caret" regardless of column.
fn indent_tab(state: &EditorState, view: &mut ViewState) -> Action {
    insert_text(state, view, "    ")
}

/// Shift-Tab: for every line that intersects the selection, remove up to 4
/// leading whitespace bytes (spaces, or a single leading tab counted as 4).
fn shift_tab_outdent(state: &EditorState, view: &mut ViewState) -> Action {
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
    Action::Replace(state.apply(tx))
}

fn delete_forward(state: &EditorState) -> Transaction {
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

fn copy_selection(state: &EditorState) -> Action {
    let mut out = String::new();
    for (i, r) in state.selection.ranges().iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !r.is_empty() {
            out.push_str(&state.doc.slice(r.range()).to_string());
        } else {
            // VSCode: with no selection, copy the whole line including newline.
            let line = state.doc.byte_to_line(r.start());
            let start = state.doc.line_to_byte(line);
            let end = if line + 1 < state.doc.len_lines() {
                state.doc.line_to_byte(line + 1)
            } else {
                state.doc.len_bytes()
            };
            out.push_str(&state.doc.slice(start..end).to_string());
        }
    }
    Action::Copy(out)
}

fn apply_selection(state: &EditorState, sel: Selection) -> EditorState {
    let tx = Transaction::new(ChangeSet::empty(state.doc.len_bytes())).with_selection(sel);
    state.apply(tx)
}

fn handle_ime(state: &EditorState, view: &mut ViewState, ev: &ImeEvent) -> Action {
    match ev {
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
                Action::Replace(state.apply(tx))
            }
        }
    }
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
            mouse_down(state, view, *x, *y, *click_count, mods)
        }
        MouseEvent::Drag { x, y, button: MouseButton::Left } => {
            mouse_drag(state, view, *x, *y)
        }
        MouseEvent::Up { button: MouseButton::Left, x, y } => {
            mouse_up(state, view, *x, *y)
        }
        _ => Action::None,
    }
}

fn mouse_down(
    state: &EditorState,
    view: &mut ViewState,
    x: f32,
    y: f32,
    click_count: u8,
    mods: Modifiers,
) -> Action {
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
        return Action::Replace(apply_selection(state, Selection::single(pos)));
    }

    view.drag = DragState::MaybeSelecting { anchor: pos };
    let sel = match click_count {
        2 => select_word_at(state, pos),
        3 => select_line_at(state, pos),
        _ if mods.alt || (mods.primary() && !mods.shift) => {
            multicursor::add_cursor(state, pos)
        }
        _ if mods.shift => {
            let anchor = state.selection.main().anchor.offset();
            Selection::from_range(SelRange::new(anchor, pos))
        }
        _ => Selection::single(pos),
    };
    Action::Replace(apply_selection(state, sel))
}

fn mouse_drag(state: &EditorState, view: &mut ViewState, x: f32, y: f32) -> Action {
    match view.drag {
        DragState::MaybeSelecting { anchor } => {
            view.touch();
            let head = view_to_buffer(state, view, x, y);
            let sel = Selection::from_range(SelRange::new(anchor, head));
            Action::Replace(apply_selection(state, sel))
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
            let sel = compute_rectangle_selection(state, view, start_xy, (x, y));
            Action::Replace(apply_selection(state, sel))
        }
        DragState::Idle => Action::None,
    }
}

/// Build a multi-range Selection covering one SelRange per buffer line
/// intersecting the vertical span `[start_xy.1, cur_xy.1]`, each spanning
/// from x→byte(min_x) to x→byte(max_x) on its own line. The main range
/// is the last one (the line the pointer is currently on, clamped).
fn compute_rectangle_selection(
    state: &EditorState,
    view: &ViewState,
    start_xy: (f32, f32),
    cur_xy: (f32, f32),
) -> Selection {
    let (sx, sy) = start_xy;
    let (cx, cy) = cur_xy;
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
    // Main range = the line under the current pointer.
    let cur_line = view
        .height_map
        .line_at_y(cy + view.scroll_y)
        .min(state.doc.len_lines().saturating_sub(1));
    let main = cur_line.saturating_sub(line_lo).min(ranges.len() - 1);
    Selection::from_ranges(ranges, main)
}

/// Map a widget-local `x` to a byte offset on buffer `line`. Mirrors
/// `view_to_buffer`'s x→column approximation (mono-width using
/// `font_size * 0.55`), but takes the line explicitly so callers building
/// rectangle selections can iterate rows without recomputing y mapping.
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
    let col_x = (x - view.gutter_width).max(0.0);
    let approx_char_w = view.font_size * 0.55;
    let col = ((col_x / approx_char_w).round() as usize).min(text_no_nl.chars().count());
    let mut byte = 0usize;
    for (i, (b, _)) in text_no_nl.char_indices().enumerate() {
        if i == col {
            return line_start + b;
        }
        byte = b + text_no_nl[b..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
    }
    line_start + byte
}

fn mouse_up(state: &EditorState, view: &mut ViewState, x: f32, y: f32) -> Action {
    let prev = view.drag;
    view.drag = DragState::Idle;
    match prev {
        DragState::DraggingSelection { drop_caret } => finish_text_drag(state, view, drop_caret),
        DragState::MaybeDraggingSelection { .. } => {
            // No drag occurred — treat as a plain click: collapse the
            // selection to a single caret at the clicked position.
            let pos = view_to_buffer(state, view, x, y);
            view.touch();
            Action::Replace(apply_selection(state, Selection::single(pos)))
        }
        _ => Action::None,
    }
}

/// Apply a text drag: remove the main selection range and reinsert it at
/// `drop_caret`. If the drop falls inside the original range, cancel.
fn finish_text_drag(state: &EditorState, view: &mut ViewState, drop_caret: usize) -> Action {
    let src = state.selection.main().range();
    if drop_caret >= src.start && drop_caret <= src.end {
        // Drop landed inside the original selection — cancel.
        view.touch();
        return Action::None;
    }
    let text = state.doc.slice(src.clone()).to_string();
    let len = text.len();
    let mut edits: Vec<(std::ops::Range<usize>, String)> = if drop_caret > src.end {
        vec![(drop_caret..drop_caret, text), (src.clone(), String::new())]
    } else {
        // drop_caret < src.start
        vec![(src.clone(), String::new()), (drop_caret..drop_caret, text)]
    };
    edits.sort_by_key(|(r, _)| r.start);
    // The ChangeSet builder expects edits in ascending order; the above
    // ordering is for our own bookkeeping. Apply via ChangeSet::of.
    let changes = ChangeSet::of(state.doc.len_bytes(), edits);
    // Final selection covers the moved text at its new location.
    let new_start = if drop_caret > src.end { drop_caret - (src.end - src.start) } else { drop_caret };
    let new_sel = Selection::from_range(SelRange::new(new_start, new_start + len));
    let tx = Transaction::new(changes)
        .with_edit_type(EditType::Other)
        .with_selection(new_sel);
    view.touch();
    Action::Replace(state.apply(tx))
}

fn pos_in_any_nonempty_range(state: &EditorState, pos: usize) -> bool {
    state.selection.ranges().iter().any(|r| !r.is_empty() && pos >= r.start() && pos < r.end())
}

fn scroll_by(view: &mut ViewState, delta_y: f32) {
    view.scroll_y = (view.scroll_y - delta_y).max(0.0);
    let max = (view.height_map.total_height() - view.height
        + view.scroll_past_end * view.height)
        .max(0.0);
    if view.scroll_y > max {
        view.scroll_y = max;
    }
}

/// Map widget-local (x, y) to a byte offset in the doc. `x` is widget-local
/// (including gutter); the host must subtract the gutter before passing if it
/// wants text-area coordinates. The view's `gutter_width` is used to clamp.
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

    let col_x = (x - view.gutter_width).max(0.0);
    // Use the measured monospace "M" width that the renderer cached on
    // `wrap_map` from a real font layout. The previous
    // `font_size * 0.55` heuristic systematically mispredicted column
    // positions on every line and got worse for any line whose galley
    // measured differently from the heuristic — clicks landed several
    // characters off the pointer, especially on long lines.
    let measured = view.wrap_map.char_width();
    let approx_char_w = if measured > 0.5 {
        measured
    } else {
        view.font_size * 0.6
    };
    let col = ((col_x / approx_char_w).round() as usize).min(vline_text.chars().count());
    let mut byte = 0usize;
    for (i, (b, _)) in vline_text.char_indices().enumerate() {
        if i == col {
            return line_start + vline_start_byte + b;
        }
        byte = b + vline_text[b..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
    }
    line_start + vline_start_byte + byte
}

fn select_word_at(state: &EditorState, pos: usize) -> Selection {
    let line = state.doc.byte_to_line(pos);
    let line_start = state.doc.line_to_byte(line);
    let text = state.doc.line_str(line);
    let local = pos - line_start;
    use unicode_segmentation::UnicodeSegmentation;
    for (i, w) in text.unicode_word_indices() {
        let end = i + w.len();
        if local >= i && local <= end {
            return Selection::from_range(SelRange::new(line_start + i, line_start + end));
        }
    }
    Selection::single(pos)
}

fn select_line_at(state: &EditorState, pos: usize) -> Selection {
    let line = state.doc.byte_to_line(pos);
    let start = state.doc.line_to_byte(line);
    let end = if line + 1 < state.doc.len_lines() {
        state.doc.line_to_byte(line + 1)
    } else {
        state.doc.len_bytes()
    };
    Selection::from_range(SelRange::new(start, end))
}

/// Helper exposed for backends that need to construct text-insertion actions
/// directly (e.g. on receipt of platform `Text` events that arrive separately
/// from key events).
pub fn insert_smol(state: &EditorState, view: &mut ViewState, s: SmolStr) -> Action {
    insert_text(state, view, &s)
}
